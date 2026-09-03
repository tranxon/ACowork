/**
 * cacheHitRate tests (ADR-066 §6).
 *
 * The cache-hit ratio is a UX metric, not a billing primitive.  We
 * just need to ensure the four contracts below hold:
 *
 *   1. `getCacheProtocol` classifies known provider ids into the
 *      right family; unknown ids return `null` rather than guessing.
 *
 *   2. `computeCacheHitRate` / `computeCacheHitStats` support two
 *      windows: **per-turn** (the last LLM call — used by the
 *      input-box usage popover) and **cumulative** (the
 *      session-lifetime totals — used by the session-status block and
 *      the bottom status bar).  The `window` argument selects which
 *      fields feed the formula.
 *
 *   3. OpenAI / Anthropic use **different** denominators — if we
 *      accidentally apply the Anthropic formula to OpenAI the hit
 *      rate gets understated; if we apply the OpenAI formula to
 *      Anthropic the hit rate gets overstated because cache_write
 *      is silently dropped.
 *
 *   4. `null` is the single "no signal" value, used in every case
 *      where the ratio is undefined (wrong provider family,
 *      zero-denominator, NaN, missing read, etc.).  Callers can
 *      render nothing or a dash without further branching.
 */
import { describe, it, expect } from "vitest";
import {
  computeCacheHitRate,
  computeCacheHitStats,
  formatCacheHitRate,
  getCacheProtocol,
  hasCacheData,
  type CacheUsageLike,
} from "./cacheHitRate";

describe("getCacheProtocol", () => {
  it("classifies OpenAI Chat Completions protocol family", () => {
    expect(getCacheProtocol("openai")).toBe("openai");
    expect(getCacheProtocol("azure")).toBe("openai");
    expect(getCacheProtocol("azure-openai")).toBe("openai");
  });

  it("classifies Anthropic Messages protocol family", () => {
    expect(getCacheProtocol("anthropic")).toBe("anthropic");
    // Bedrock hosts Anthropic models with the same cache accounting
    // shape; non-Anthropic Bedrock models keep cache counters at 0
    // and the denominator-zero guard handles them correctly.
    expect(getCacheProtocol("bedrock")).toBe("anthropic");
  });

  it("returns null for providers without cache accounting", () => {
    expect(getCacheProtocol("ollama")).toBeNull();
    expect(getCacheProtocol("ollama-local")).toBeNull();
    expect(getCacheProtocol("lmstudio")).toBeNull();
    expect(getCacheProtocol("deepseek")).toBeNull();
    expect(getCacheProtocol("zhipuai")).toBeNull();
    expect(getCacheProtocol("minimax")).toBeNull();
    expect(getCacheProtocol("minimax-cn-coding-plan")).toBeNull();
    expect(getCacheProtocol("volcengine-agent-plan")).toBeNull();
  });

  it("returns null for missing or empty provider id", () => {
    expect(getCacheProtocol(null)).toBeNull();
    expect(getCacheProtocol(undefined)).toBeNull();
    expect(getCacheProtocol("")).toBeNull();
  });

  it("does not over-classify unknown ids by prefix", () => {
    // `openai-compatible` would be a real OpenAI-compatible endpoint
    // — but since user-defined custom providers can use *any* id,
    // we err on the side of not auto-classifying and require
    // explicit registration.  See ADR-066 §6 and the discussion in
    // cacheHitRate.ts.
    expect(getCacheProtocol("openai-compatible")).toBeNull();
    expect(getCacheProtocol("my-custom-provider")).toBeNull();
  });
});

describe("computeCacheHitRate — OpenAI formula", () => {
  // OpenAI denominator: cache_read / prompt_tokens (no write event).
  const openaiProvider = "openai";

  it("computes the per-turn hit rate when cumulative is unavailable", () => {
    const usage: CacheUsageLike = {
      cache_read_tokens: 400,
      cache_write_tokens: 0, // OpenAI never reports writes
      input_tokens: 1000,
    };
    expect(computeCacheHitRate(openaiProvider, usage)).toBeCloseTo(
      400 / 1000,
      10,
    );
  });

  it("uses per-turn numbers even when cumulative is present (real-time view)", () => {
    const usage: CacheUsageLike = {
      // The ratio is the *current* LLM call's real-time view, so it
      // uses per-turn counts (400 / 600 = 66.7%) — NOT the session
      // lifetime (600 / 1800 = 33.3%).  Cumulative is deferred to a
      // future cost-analysis surface.
      cache_read_tokens: 400,
      input_tokens: 600,
      total_cache_read_tokens: 600,
      total_input_tokens: 1800,
    };
    expect(computeCacheHitRate(openaiProvider, usage)).toBeCloseTo(
      400 / 600,
      10,
    );
  });

  it("ignores cache_write_tokens (OpenAI does not have the concept)", () => {
    const usage: CacheUsageLike = {
      cache_read_tokens: 500,
      cache_write_tokens: 999_999, // should be ignored
      input_tokens: 1000,
    };
    expect(computeCacheHitRate(openaiProvider, usage)).toBeCloseTo(
      500 / 1000,
      10,
    );
  });

  it("returns null when prompt_tokens is zero", () => {
    const usage: CacheUsageLike = {
      cache_read_tokens: 100,
      input_tokens: 0,
    };
    expect(computeCacheHitRate(openaiProvider, usage)).toBeNull();
  });

  it("returns null when prompt_tokens is missing", () => {
    const usage: CacheUsageLike = { cache_read_tokens: 100 };
    expect(computeCacheHitRate(openaiProvider, usage)).toBeNull();
  });
});

describe("computeCacheHitRate — Anthropic formula", () => {
  // Anthropic denominator: cache_read / (input + cache_read + cache_write).
  const anthropicProvider = "anthropic";

  it("computes the Anthropic hit rate with all three components", () => {
    const usage: CacheUsageLike = {
      cache_read_tokens: 400,
      cache_write_tokens: 200,
      input_tokens: 1000,
    };
    expect(computeCacheHitRate(anthropicProvider, usage)).toBeCloseTo(
      400 / (400 + 200 + 1000),
      10,
    );
  });

  it("computes with cache_write=0 (steady state after seeding)", () => {
    const usage: CacheUsageLike = {
      cache_read_tokens: 400,
      cache_write_tokens: 0,
      input_tokens: 1000,
    };
    expect(computeCacheHitRate(anthropicProvider, usage)).toBeCloseTo(
      400 / 1400,
      10,
    );
  });

  it("uses per-turn numbers even when cumulative is present (real-time view)", () => {
    const usage: CacheUsageLike = {
      cache_read_tokens: 100,
      cache_write_tokens: 50,
      input_tokens: 200,
      total_cache_read_tokens: 600,
      total_cache_write_tokens: 100,
      total_input_tokens: 1300,
    };
    expect(computeCacheHitRate(anthropicProvider, usage)).toBeCloseTo(
      100 / (100 + 50 + 200),
      10,
    );
  });

  it("works for Bedrock-hosted Anthropic models", () => {
    const usage: CacheUsageLike = {
      cache_read_tokens: 300,
      cache_write_tokens: 100,
      input_tokens: 600,
    };
    expect(computeCacheHitRate("bedrock", usage)).toBeCloseTo(
      300 / (300 + 100 + 600),
      10,
    );
  });

  it("returns null when the combined denominator is zero", () => {
    const usage: CacheUsageLike = {
      cache_read_tokens: 0,
      cache_write_tokens: 0,
      input_tokens: 0,
    };
    expect(computeCacheHitRate(anthropicProvider, usage)).toBeNull();
  });
});

describe("computeCacheHitRate — no-signal cases", () => {
  it("returns null when cache_read is zero or negative", () => {
    expect(
      computeCacheHitRate("openai", {
        cache_read_tokens: 0,
        input_tokens: 1000,
      }),
    ).toBeNull();
    expect(
      computeCacheHitRate("openai", {
        cache_read_tokens: -5,
        input_tokens: 1000,
      }),
    ).toBeNull();
  });

  it("returns null when cache_read is missing", () => {
    expect(
      computeCacheHitRate("openai", { input_tokens: 1000 }),
    ).toBeNull();
    expect(
      computeCacheHitRate("openai", { total_cache_read_tokens: 100 }),
    ).toBeNull(); // per-turn fallback also missing
  });

  it("returns null when the data shows no cache accounting", () => {
    // `ollama` never reports cache tokens — cache_read stays 0, so the
    // ratio is undefined regardless of provider id.
    expect(
      computeCacheHitRate("ollama", {
        cache_read_tokens: 0,
        input_tokens: 200,
      }),
    ).toBeNull();
    expect(
      computeCacheHitRate("ollama", { input_tokens: 200 }),
    ).toBeNull();
  });

  it("computes the ratio for OpenAI-compatible providers that report cached_tokens", () => {
    // MiniMax / Volcengine / custom providers route through the
    // Runtime's OpenAIProvider and surface
    // `prompt_tokens_details.cached_tokens`.  The ratio is well-defined
    // even though the provider id isn't in the explicit allowlist —
    // the formula is chosen from the data, not the id.
    expect(
      computeCacheHitRate("minimax-cn-coding-plan", {
        cache_read_tokens: 400,
        input_tokens: 1000,
      }),
    ).toBeCloseTo(0.4, 10);
    expect(
      computeCacheHitRate("volcengine-agent-plan", {
        cache_read_tokens: 400,
        input_tokens: 1000,
      }),
    ).toBeCloseTo(0.4, 10);
    expect(
      computeCacheHitRate("custom-agnes", {
        cache_read_tokens: 400,
        input_tokens: 1000,
      }),
    ).toBeCloseTo(0.4, 10);
  });

  it("uses the Anthropic formula for unknown providers that report cache writes", () => {
    // A custom provider reporting both read and write is Anthropic-style
    // (the Runtime's OpenAI-compatible providers never report writes).
    expect(
      computeCacheHitRate("custom-anthropic-gateway", {
        cache_read_tokens: 400,
        cache_write_tokens: 200,
        input_tokens: 1000,
      }),
    ).toBeCloseTo(400 / (400 + 200 + 1000), 10);
  });

  it("returns null when usage is null or undefined", () => {
    expect(computeCacheHitRate("openai", null)).toBeNull();
    expect(computeCacheHitRate("anthropic", undefined)).toBeNull();
  });

  it("treats NaN / Infinity in numeric fields as missing", () => {
    expect(
      computeCacheHitRate("openai", {
        cache_read_tokens: Number.NaN,
        input_tokens: 1000,
      }),
    ).toBeNull();
    expect(
      computeCacheHitRate("openai", {
        cache_read_tokens: 100,
        input_tokens: Number.POSITIVE_INFINITY,
      }),
    ).toBeNull();
  });
});

describe("computeCacheHitRate — cumulative (session) window", () => {
  it("uses session-lifetime totals for OpenAI-style providers", () => {
    const usage: CacheUsageLike = {
      cache_read_tokens: 400, // per-turn — ignored in the cumulative window
      input_tokens: 600,
      total_cache_read_tokens: 600,
      total_input_tokens: 1800,
    };
    expect(computeCacheHitRate("openai", usage, "cumulative")).toBeCloseTo(
      600 / 1800,
      10,
    );
  });

  it("uses session-lifetime totals for Anthropic-style providers", () => {
    const usage: CacheUsageLike = {
      cache_read_tokens: 100,
      cache_write_tokens: 50,
      input_tokens: 200,
      total_cache_read_tokens: 600,
      total_cache_write_tokens: 100,
      total_input_tokens: 1300,
    };
    expect(computeCacheHitRate("anthropic", usage, "cumulative")).toBeCloseTo(
      600 / (600 + 100 + 1300),
      10,
    );
  });

  it("returns null when cumulative read is zero", () => {
    expect(
      computeCacheHitRate(
        "openai",
        { total_cache_read_tokens: 0, total_input_tokens: 1000 },
        "cumulative",
      ),
    ).toBeNull();
  });

  it("returns null when cumulative input is missing", () => {
    expect(
      computeCacheHitRate("openai", { total_cache_read_tokens: 100 }, "cumulative"),
    ).toBeNull();
  });
});

describe("hasCacheData", () => {
  it("returns true when any per-turn cache field is present and positive", () => {
    expect(hasCacheData({ cache_read_tokens: 400 })).toBe(true);
    expect(hasCacheData({ cache_write_tokens: 200 })).toBe(true);
  });

  it("per-turn window also reports true when only cumulative is present (0-hit round)", () => {
    // A 0-hit round in a cache-active session still renders the row
    // (as `— | 0 / N`) rather than hiding it.
    expect(hasCacheData({ total_cache_read_tokens: 600 })).toBe(true);
    expect(hasCacheData({ total_cache_write_tokens: 100 })).toBe(true);
  });

  it("cumulative window checks only the session-lifetime totals", () => {
    expect(hasCacheData({ total_cache_read_tokens: 600 }, "cumulative")).toBe(true);
    expect(hasCacheData({ total_cache_write_tokens: 100 }, "cumulative")).toBe(true);
    // Per-turn-only data does not gate the cumulative surface.
    expect(hasCacheData({ cache_read_tokens: 400 }, "cumulative")).toBe(false);
    expect(hasCacheData({ cache_write_tokens: 200 }, "cumulative")).toBe(false);
  });

  it("returns false when no cache accounting is present", () => {
    expect(hasCacheData(null)).toBe(false);
    expect(hasCacheData(undefined)).toBe(false);
    expect(hasCacheData({})).toBe(false);
    expect(hasCacheData({ input_tokens: 1000 })).toBe(false);
    expect(hasCacheData({ cache_read_tokens: 0, cache_write_tokens: 0 })).toBe(false);
    expect(hasCacheData({ total_cache_read_tokens: 0 }, "cumulative")).toBe(false);
  });
});

describe("computeCacheHitStats", () => {
  it("returns numerator = cache-hit tokens and denominator = input tokens (OpenAI)", () => {
    const stats = computeCacheHitStats("minimax-cn-coding-plan", {
      cache_read_tokens: 400,
      input_tokens: 1000,
    });
    expect(stats.numerator).toBe(400);
    expect(stats.denominator).toBe(1000);
    expect(stats.ratio).toBeCloseTo(0.4, 10);
  });

  it("uses per-turn numbers even when cumulative is present (real-time view)", () => {
    const stats = computeCacheHitStats("openai", {
      cache_read_tokens: 400,
      input_tokens: 600,
      total_cache_read_tokens: 600,
      total_input_tokens: 1800,
    });
    expect(stats.numerator).toBe(400);
    expect(stats.denominator).toBe(600);
    expect(stats.ratio).toBeCloseTo(400 / 600, 10);
  });

  it("includes cache-write tokens in the Anthropic denominator", () => {
    const stats = computeCacheHitStats("anthropic", {
      cache_read_tokens: 400,
      cache_write_tokens: 200,
      input_tokens: 1000,
    });
    expect(stats.numerator).toBe(400);
    expect(stats.denominator).toBe(400 + 200 + 1000);
    expect(stats.ratio).toBeCloseTo(400 / (400 + 200 + 1000), 10);
  });

  it("returns zeroed stats when there is no usage", () => {
    expect(computeCacheHitStats("openai", null)).toEqual({
      ratio: null,
      numerator: 0,
      denominator: 0,
    });
  });

  it("keeps numerator/denominator consistent with the ratio for unknown providers", () => {
    // A custom Anthropic-style gateway reporting writes.
    const stats = computeCacheHitStats("custom-anthropic-gateway", {
      cache_read_tokens: 300,
      cache_write_tokens: 100,
      input_tokens: 600,
    });
    expect(stats.numerator).toBe(300);
    expect(stats.denominator).toBe(300 + 100 + 600);
    expect(stats.ratio).toBeCloseTo(300 / (300 + 100 + 600), 10);
  });

  it("returns cumulative numerator/denominator for OpenAI-style providers", () => {
    const stats = computeCacheHitStats(
      "minimax-cn-coding-plan",
      {
        cache_read_tokens: 400,
        input_tokens: 600,
        total_cache_read_tokens: 600,
        total_input_tokens: 1800,
      },
      "cumulative",
    );
    expect(stats.numerator).toBe(600);
    expect(stats.denominator).toBe(1800);
    expect(stats.ratio).toBeCloseTo(600 / 1800, 10);
  });

  it("includes cumulative cache-write tokens in the Anthropic denominator", () => {
    const stats = computeCacheHitStats(
      "anthropic",
      {
        total_cache_read_tokens: 600,
        total_cache_write_tokens: 100,
        total_input_tokens: 1300,
      },
      "cumulative",
    );
    expect(stats.numerator).toBe(600);
    expect(stats.denominator).toBe(600 + 100 + 1300);
    expect(stats.ratio).toBeCloseTo(600 / (600 + 100 + 1300), 10);
  });
});

describe("edge scenarios — context compaction & session resume", () => {
  // After compaction the Runtime zeroes the per-turn cache fields
  // (`set_history_anchor`) while the cumulative totals keep the
  // compaction LLM call's cache usage (`accumulate_compaction_usage`).
  // The popup (per-turn) must show a dash — there is no fresh per-turn
  // snapshot — and the session-status (cumulative) must reflect the
  // accumulated totals.

  it("after compaction: per-turn shows dash, cumulative keeps totals (OpenAI)", () => {
    const usage: CacheUsageLike = {
      // Per-turn zeroed by set_history_anchor; input = post-compaction
      // history size.
      cache_read_tokens: 0,
      cache_write_tokens: 0,
      input_tokens: 3_500,
      // Cumulative includes the compaction LLM call (full-history input).
      total_cache_read_tokens: 104_000,
      total_cache_write_tokens: 0,
      total_input_tokens: 154_000,
    };
    // Popup (per-turn): no fresh cache snapshot → dash.
    const perTurn = computeCacheHitStats("minimax-cn-coding-plan", usage);
    expect(perTurn.ratio).toBeNull();
    expect(perTurn.numerator).toBe(0);
    expect(perTurn.denominator).toBe(3_500);
    // Session-status (cumulative): OpenAI formula.
    const cumulative = computeCacheHitStats(
      "minimax-cn-coding-plan",
      usage,
      "cumulative",
    );
    expect(cumulative.numerator).toBe(104_000);
    expect(cumulative.denominator).toBe(154_000);
    expect(cumulative.ratio).toBeCloseTo(104_000 / 154_000, 10);
  });

  it("after compaction: per-turn shows dash, cumulative keeps totals (Anthropic)", () => {
    const usage: CacheUsageLike = {
      cache_read_tokens: 0,
      cache_write_tokens: 0,
      input_tokens: 3_500,
      total_cache_read_tokens: 104_000,
      total_cache_write_tokens: 800,
      total_input_tokens: 154_000,
    };
    const perTurn = computeCacheHitStats("anthropic", usage);
    expect(perTurn.ratio).toBeNull();
    expect(perTurn.denominator).toBe(3_500);
    const cumulative = computeCacheHitStats("anthropic", usage, "cumulative");
    expect(cumulative.numerator).toBe(104_000);
    expect(cumulative.denominator).toBe(104_000 + 800 + 154_000);
    expect(cumulative.ratio).toBeCloseTo(
      104_000 / (104_000 + 800 + 154_000),
      10,
    );
  });

  it("after compaction: hasCacheData gates per-turn on cumulative (0-hit round still shows)", () => {
    const usage: CacheUsageLike = {
      cache_read_tokens: 0,
      cache_write_tokens: 0,
      input_tokens: 3_500,
      total_cache_read_tokens: 104_000,
      total_cache_write_tokens: 800,
    };
    // Popup gate: the session has seen cache → row still renders as "— | 0 / N".
    expect(hasCacheData(usage)).toBe(true);
    // Session-status gate: cumulative totals non-zero.
    expect(hasCacheData(usage, "cumulative")).toBe(true);
  });

  it("after compaction with no prior cache: both windows hide the row", () => {
    const usage: CacheUsageLike = {
      cache_read_tokens: 0,
      cache_write_tokens: 0,
      input_tokens: 3_500,
      total_cache_read_tokens: 0,
      total_cache_write_tokens: 0,
    };
    expect(hasCacheData(usage)).toBe(false);
    expect(hasCacheData(usage, "cumulative")).toBe(false);
  });

  // Historical session resume: the Runtime rebuilds ContextUsageInfo from
  // persisted SessionTokens (`build_context_usage_from_persisted`), so both
  // per-turn (last call) and cumulative (session lifetime) cache fields are
  // populated before the first new conversation.

  it("resumed session: popup shows the historical last call's per-turn rate", () => {
    const usage: CacheUsageLike = {
      cache_read_tokens: 4_000,
      cache_write_tokens: 800,
      input_tokens: 10_000,
      total_cache_read_tokens: 104_000,
      total_cache_write_tokens: 800,
      total_input_tokens: 154_000,
    };
    const perTurn = computeCacheHitStats("anthropic", usage);
    expect(perTurn.numerator).toBe(4_000);
    expect(perTurn.denominator).toBe(4_000 + 800 + 10_000);
    expect(perTurn.ratio).toBeCloseTo(4_000 / (4_000 + 800 + 10_000), 10);
    const cumulative = computeCacheHitStats("anthropic", usage, "cumulative");
    expect(cumulative.numerator).toBe(104_000);
    expect(cumulative.denominator).toBe(104_000 + 800 + 154_000);
  });

  it("first conversation after resume: per-turn reflects the new call, cumulative adds to restored totals", () => {
    // New call re-seeds the cache (Anthropic): read=0, write=5000, prompt=15000.
    const usage: CacheUsageLike = {
      cache_read_tokens: 0,
      cache_write_tokens: 5_000,
      input_tokens: 15_000,
      total_cache_read_tokens: 104_000,
      total_cache_write_tokens: 5_800,
      total_input_tokens: 169_000,
    };
    const perTurn = computeCacheHitStats("anthropic", usage);
    expect(perTurn.ratio).toBeNull(); // read=0 → no hit this round
    expect(perTurn.denominator).toBe(15_000 + 5_000);
    const cumulative = computeCacheHitStats("anthropic", usage, "cumulative");
    expect(cumulative.numerator).toBe(104_000);
    expect(cumulative.denominator).toBe(104_000 + 5_800 + 169_000);
    expect(cumulative.ratio).toBeCloseTo(
      104_000 / (104_000 + 5_800 + 169_000),
      10,
    );
  });
});

describe("formatCacheHitRate", () => {
  it("renders as an integer percentage (0 decimals, matching context usage)", () => {
    // Truncation (floor) — same rationale as `formatPercent`: a
    // sub-100% ratio must NOT round up to "100%", otherwise the
    // bar reads as "full cache" while the live token counts above
    // are visibly less than full.
    expect(formatCacheHitRate(0)).toBe("0%");
    expect(formatCacheHitRate(0.5)).toBe("50%");
    expect(formatCacheHitRate(0.123)).toBe("12%");
    expect(formatCacheHitRate(0.896)).toBe("89%");
    expect(formatCacheHitRate(1)).toBe("100%");
  });

  it("clamps visually-out-of-range ratios to [0%, 100%]", () => {
    // Floating-point ratios from `computeCacheHitRate` are already
    // clamped, but defensive formatting protects direct callers.
    expect(formatCacheHitRate(-0.5)).toBe("0%");
    expect(formatCacheHitRate(1.5)).toBe("100%");
  });

  it("returns null for null input (no-signal pass-through)", () => {
    expect(formatCacheHitRate(null)).toBeNull();
  });
});