/**
 * cacheHitRate tests (ADR-066 §6).
 *
 * The cache-hit ratio is a UX metric, not a billing primitive.  We
 * just need to ensure the four contracts below hold:
 *
 *   1. `getCacheProtocol` classifies known provider ids into the
 *      right family; unknown ids return `null` rather than guessing.
 *
 *   2. `computeCacheHitRate` picks the **cumulative** numbers when
 *      both per-turn and cumulative are present (the status panel
 *      shows the lifetime hit rate), and falls back to per-turn
 *      when cumulative isn't available yet (the very first LLM
 *      call's push).
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
  formatCacheHitRate,
  getCacheProtocol,
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

  it("prefers cumulative numbers over per-turn when both are present", () => {
    const usage: CacheUsageLike = {
      // per-turn shows a recent big hit (400 / 600 = 66.7%),
      // cumulative shows the lifetime view (600 / 1800 = 33.3%).
      // The status panel wants the lifetime view.
      cache_read_tokens: 400,
      input_tokens: 600,
      total_cache_read_tokens: 600,
      total_input_tokens: 1800,
    };
    expect(computeCacheHitRate(openaiProvider, usage)).toBeCloseTo(
      600 / 1800,
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

  it("prefers cumulative numbers over per-turn", () => {
    const usage: CacheUsageLike = {
      cache_read_tokens: 100,
      cache_write_tokens: 50,
      input_tokens: 200,
      total_cache_read_tokens: 600,
      total_cache_write_tokens: 100,
      total_input_tokens: 1300,
    };
    expect(computeCacheHitRate(anthropicProvider, usage)).toBeCloseTo(
      600 / (600 + 100 + 1300),
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

  it("returns null for providers without cache accounting", () => {
    // `ollama` has no cache_read / cache_write semantics at all.
    expect(
      computeCacheHitRate("ollama", {
        cache_read_tokens: 100,
        input_tokens: 200,
      }),
    ).toBeNull();
    expect(
      computeCacheHitRate("deepseek", {
        cache_read_tokens: 100,
        input_tokens: 200,
      }),
    ).toBeNull();
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

describe("formatCacheHitRate", () => {
  it("renders with one decimal place", () => {
    expect(formatCacheHitRate(0)).toBe("0.0%");
    expect(formatCacheHitRate(0.5)).toBe("50.0%");
    expect(formatCacheHitRate(0.123)).toBe("12.3%");
    expect(formatCacheHitRate(1)).toBe("100.0%");
  });

  it("clamps visually-out-of-range ratios to [0%, 100%]", () => {
    // Floating-point ratios from `computeCacheHitRate` are already
    // clamped, but defensive formatting protects direct callers.
    expect(formatCacheHitRate(-0.5)).toBe("0.0%");
    expect(formatCacheHitRate(1.5)).toBe("100.0%");
  });

  it("returns null for null input (no-signal pass-through)", () => {
    expect(formatCacheHitRate(null)).toBeNull();
  });
});