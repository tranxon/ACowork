/**
 * httpRetry tests.
 *
 * Regression coverage for Bug B v3 (§4.13 of `docs/zh/protocols/http.md`):
 * every store-level fetcher retries Gateway 503s until the Gateway is
 * Ready. Two contracts are locked in here:
 *
 *   - `parseRetryAfterMs` decodes the Gateway's `Retry-After` header,
 *     including the `-1` SHUTTING_DOWN sentinel. A naive `seconds * 1000`
 *     would turn `-1` into `-1000` and the abort check (`=== -1`) would
 *     never fire — the sentinel must pass through verbatim.
 *   - `with503Retry` stops on non-503, honours the sentinel, respects
 *     maxRetries / wall-clock budget, and threads AbortSignal.
 */

import { describe, it, expect, vi, afterEach } from "vitest";
import { parseRetryAfterMs, with503Retry } from "./httpRetry";

// ── Fixtures ─────────────────────────────────────────────────────────────

/** Minimal Response stub — only what with503Retry touches. */
function mockResp(status: number, retryAfter: string | null): Response {
    const headers = new Headers();
    if (retryAfter !== null) headers.set("retry-after", retryAfter);
    return {
        status,
        ok: status >= 200 && status < 300,
        headers,
        json: () => Promise.resolve({}),
    } as unknown as Response;
}

afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
});

// ── parseRetryAfterMs ────────────────────────────────────────────────────

describe("parseRetryAfterMs", () => {
    it("returns null for absent / empty header values", () => {
        expect(parseRetryAfterMs(null)).toBeNull();
        expect(parseRetryAfterMs("")).toBeNull();
        expect(parseRetryAfterMs("   ")).toBeNull();
    });

    it("parses delta-seconds form (the form the Gateway emits)", () => {
        expect(parseRetryAfterMs("5")).toBe(5000);
        expect(parseRetryAfterMs("2")).toBe(2000);
        expect(parseRetryAfterMs("10")).toBe(10_000);
        // Whitespace around the value is tolerated.
        expect(parseRetryAfterMs(" 3 ")).toBe(3000);
        // 0 means "retry immediately" — still a valid hint, not null.
        expect(parseRetryAfterMs("0")).toBe(0);
    });

    it("passes the -1 SHUTTING_DOWN sentinel through verbatim", () => {
        // Regression: `-1 * 1000` would return -1000 and the abort check
        // (`retryAfterMs === -1`) in with503Retry would never fire.
        expect(parseRetryAfterMs("-1")).toBe(-1);
        expect(parseRetryAfterMs(" -1 ")).toBe(-1);
    });

    it("parses HTTP-date form as a fallback (RFC 7231 §7.1.3)", () => {
        vi.useFakeTimers();
        vi.setSystemTime(new Date("2026-08-28T00:00:00Z"));

        const future = new Date("2026-08-28T00:01:00Z").toUTCString();
        expect(parseRetryAfterMs(future)).toBe(60_000);

        // A date in the past means "retry now" — clamped to 0.
        const past = new Date("2026-08-27T00:00:00Z").toUTCString();
        expect(parseRetryAfterMs(past)).toBe(0);
    });

    it("returns null for unparseable values", () => {
        expect(parseRetryAfterMs("abc")).toBeNull();
        expect(parseRetryAfterMs("soon")).toBeNull();
        expect(parseRetryAfterMs("1.5.3")).toBeNull();
    });
});

// ── with503Retry ─────────────────────────────────────────────────────────

describe("with503Retry", () => {
    it("passes through non-503 responses untouched", async () => {
        const fetchMock = vi.fn(async () => mockResp(404, null));
        vi.stubGlobal("fetch", fetchMock);

        const resp = await with503Retry(() => fetch("x"));

        expect(resp.status).toBe(404);
        expect(fetchMock).toHaveBeenCalledTimes(1);
    });

    it("retries a 503 after Retry-After and returns the eventual 200", async () => {
        vi.useFakeTimers();
        const fetchMock = vi
            .fn()
            .mockResolvedValueOnce(mockResp(503, "2"))
            .mockResolvedValueOnce(mockResp(200, null));
        vi.stubGlobal("fetch", fetchMock);

        const promise = with503Retry(() => fetch("x"), { tag: "test" });
        await vi.advanceTimersByTimeAsync(5000); // covers the 2s sleep

        const resp = await promise;
        expect(resp.status).toBe(200);
        expect(fetchMock).toHaveBeenCalledTimes(2);
    });

    it("aborts immediately on Retry-After: -1 (SHUTTING_DOWN sentinel)", async () => {
        const fetchMock = vi.fn(async () => mockResp(503, "-1"));
        vi.stubGlobal("fetch", fetchMock);

        const resp = await with503Retry(() => fetch("x"));

        // The LATEST response is returned; callers must surface it.
        expect(resp.status).toBe(503);
        expect(fetchMock).toHaveBeenCalledTimes(1);
    });

    it("exhausts maxRetries and returns the last 503", async () => {
        vi.useFakeTimers();
        const fetchMock = vi.fn(async () => mockResp(503, "0"));
        vi.stubGlobal("fetch", fetchMock);

        const policy = {
            maxRetries: 3,
            backoffBaseMs: 100,
            backoffCapMs: 100,
            totalBudgetMs: 60_000,
        };
        const promise = with503Retry(() => fetch("x"), { policy });
        await vi.advanceTimersByTimeAsync(60_000);

        const resp = await promise;
        expect(resp.status).toBe(503);
        // initial attempt + 3 retries
        expect(fetchMock).toHaveBeenCalledTimes(4);
    });

    it("gives up once the wall-clock budget is exceeded", async () => {
        vi.useFakeTimers();
        const t0 = Date.now();
        const fetchMock = vi.fn(async () => {
            // Simulate a stuck Gateway: wall clock keeps running while
            // the response says "not ready yet".
            vi.setSystemTime(t0 + 10_000);
            return mockResp(503, "0");
        });
        vi.stubGlobal("fetch", fetchMock);

        const policy = {
            maxRetries: 100,
            backoffBaseMs: 1,
            backoffCapMs: 1,
            totalBudgetMs: 5000,
        };
        const resp = await with503Retry(() => fetch("x"), { policy });

        expect(resp.status).toBe(503);
        expect(fetchMock).toHaveBeenCalledTimes(1);
    });

    it("uses the Retry-After hint over the exponential backoff when present", async () => {
        vi.useFakeTimers();
        const fetchMock = vi
            .fn()
            .mockResolvedValueOnce(mockResp(503, "7"))
            .mockResolvedValueOnce(mockResp(200, null));
        vi.stubGlobal("fetch", fetchMock);

        const promise = with503Retry(() => fetch("x"), {
            policy: {
                maxRetries: 3,
                backoffBaseMs: 100, // would be 100ms on first retry if ignored
                backoffCapMs: 1000,
                totalBudgetMs: 60_000,
            },
        });
        // The hint says 7s — a 2s advance must NOT have fired the retry.
        await vi.advanceTimersByTimeAsync(2000);
        expect(fetchMock).toHaveBeenCalledTimes(1);
        await vi.advanceTimersByTimeAsync(6000);

        const resp = await promise;
        expect(resp.status).toBe(200);
        expect(fetchMock).toHaveBeenCalledTimes(2);
    });

    it("throws AbortError when the signal is already aborted", async () => {
        const controller = new AbortController();
        controller.abort();

        await expect(
            with503Retry(() => fetch("x"), { signal: controller.signal }),
        ).rejects.toMatchObject({ name: "AbortError" });
    });

    it("throws AbortError when aborted mid-retry", async () => {
        vi.useFakeTimers();
        const fetchMock = vi.fn(async () => mockResp(503, "5"));
        vi.stubGlobal("fetch", fetchMock);
        const controller = new AbortController();

        const promise = with503Retry(() => fetch("x"), {
            signal: controller.signal,
        });
        // Attach a no-op catch immediately: the rejection fires while
        // advanceTimersByTimeAsync flushes the sleep microtask, before
        // the assertion below can attach its handler — otherwise vitest
        // reports a (benign but noisy) unhandled rejection.
        promise.catch(() => {});
        // Abort while the loop sleeps on the 5s Retry-After backoff.
        controller.abort();
        await vi.advanceTimersByTimeAsync(5000);

        await expect(promise).rejects.toMatchObject({ name: "AbortError" });
    });

    it("propagates fetcher errors", async () => {
        const fetchMock = vi.fn(async () => {
            throw new TypeError("network down");
        });
        vi.stubGlobal("fetch", fetchMock);

        await expect(with503Retry(() => fetch("x"))).rejects.toThrow(
            "network down",
        );
        expect(fetchMock).toHaveBeenCalledTimes(1);
    });
});
