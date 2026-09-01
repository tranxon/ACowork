/**
 * formatBubbleTime tests.
 *
 * Three contracts must hold:
 *
 *   - Falsy / invalid input collapses to "" so the bubble component can
 *     skip rendering the timestamp span entirely on legacy entries.
 *   - Valid timestamps render a non-empty string.
 *   - The structural skeleton `YYYY/MM/DD HH:MM:SS` is preserved across
 *     locales (timezone-independent assertion).
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
  });

  it("renders a non-empty string for a valid timestamp", () => {
    const ms = new Date(2026, 7, 30, 12, 34, 56).getTime();
    const out = formatBubbleTime(ms);
    expect(out).not.toBe("");
    expect(out.length).toBeGreaterThan(8);
  });

  it("preserves the structural skeleton YYYY/MM/DD HH:MM:SS across locales", () => {
    // Use a local-time constructor so the assertion is independent of the
    // runner's timezone (jsdom picks up Asia/Shanghai here, CI may use UTC).
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