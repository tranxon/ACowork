/**
 * formatBubbleTime tests.
 *
 * The bubble hover tooltip uses this helper to render the per-message
 * timestamp. Three contracts must hold:
 *
 *   - Falsy / invalid input collapses to "" so it can be passed straight
 *     to `<Tooltip content={...} />` (the Tooltip short-circuits on "" and
 *     skips its wrapper DOM). Without this, a 0 / NaN / undefined timestamp
 *     would render the literal "Invalid Date" string in the tooltip.
 *   - Valid timestamps render a non-empty string.
 *   - Output is locale-aware (the exact separator characters depend on
 *     the runtime's default locale, but the structural skeleton
 *     `YYYY/MM/DD, HH:MM:SS` is preserved).
 */
import { describe, it, expect } from "vitest";
import { formatBubbleTime } from "./formatTime";

describe("formatBubbleTime", () => {
  it("returns empty string for falsy input", () => {
    expect(formatBubbleTime(undefined)).toBe("");
    expect(formatBubbleTime(null)).toBe("");
    expect(formatBubbleTime(0)).toBe("");
  });

  it("returns empty string for invalid timestamps", () => {
    expect(formatBubbleTime(Number.NaN)).toBe("");
    expect(formatBubbleTime(Number.POSITIVE_INFINITY)).toBe("");
    expect(formatBubbleTime(Number.NEGATIVE_INFINITY)).toBe("");
    expect(formatBubbleTime(-1)).not.toBe(""); // valid date (1 BCE), must not be filtered
  });

  it("renders a non-empty string for a valid timestamp", () => {
    // 2026-08-30 12:34:56 local time — the absolute instant varies with TZ
    // but the formatter always renders a non-empty locale string.
    const ms = new Date(2026, 7, 30, 12, 34, 56).getTime();
    const out = formatBubbleTime(ms);
    expect(out).not.toBe("");
    expect(out.length).toBeGreaterThan(8);
  });

  it("preserves the structural skeleton YYYY/MM/DD HH:MM:SS across locales", () => {
    // Use a local-time constructor so the assertion is independent of the
    // runner's timezone (jsdom picks up Asia/Shanghai here, CI may use UTC).
    // The structural skeleton `YYYY/MM/DD, HH:MM:SS` must hold regardless:
    // 4-digit year, 2-digit month/day, 2-digit hour/minute/second, in that
    // exact order — locale-specific separators ("/", "-", " ", U+202F, etc.)
    // may appear between fields but never inside them.
    const ms = new Date(2026, 7, 30, 12, 34, 56).getTime();
    const out = formatBubbleTime(ms);
    const digits = out.replace(/[^0-9]/g, "");
    expect(digits).toHaveLength(14); // YYYY + MM + DD + HH + MM + SS
    expect(digits.slice(0, 4)).toBe("2026");
    expect(digits.slice(4, 6)).toBe("08");
    expect(digits.slice(6, 8)).toBe("30");
    expect(digits.slice(8, 10)).toMatch(/^\d{2}$/);
    expect(digits.slice(10, 12)).toMatch(/^\d{2}$/);
    expect(digits.slice(12, 14)).toMatch(/^\d{2}$/);
  });
});
