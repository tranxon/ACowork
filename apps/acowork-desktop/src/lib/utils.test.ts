import { describe, it, expect } from "vitest";
import { formatPercent } from "./utils";

describe("formatPercent", () => {
  it("renders integers without decimals", () => {
    expect(formatPercent(0)).toBe("0");
    expect(formatPercent(50)).toBe("50");
    expect(formatPercent(100)).toBe("100");
  });

  it("floors fractional values to the integer below", () => {
    // Truncation (not rounding) — a sub-100% usage that would round up
    // to 100% must NOT display as 100%, otherwise the bar reads as
    // "full" while the live token counts above it are visibly less
    // than full (e.g. 44.6K / 44.8K → 99.55% must render as `99`,
    // not `100`).
    expect(formatPercent(89.4)).toBe("89");
    expect(formatPercent(89.6)).toBe("89");
    expect(formatPercent(99.5)).toBe("99");
    expect(formatPercent(99.9)).toBe("99");
    expect(formatPercent(44.6 / 44.8 * 100)).toBe("99");
  });

  it("guards against non-finite input", () => {
    expect(formatPercent(Number.NaN)).toBe("0");
    expect(formatPercent(Number.POSITIVE_INFINITY)).toBe("0");
  });
});
