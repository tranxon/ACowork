/**
 * fileTree — treeCache unit tests
 *
 * Locks in the behavioural contract that the previous inlined
 * implementation violated in production:
 *
 *   - concurrent `fetch(key)` calls share ONE in-flight Promise
 *     (the React 18 <StrictMode> / Zustand commit race that left
 *      `__agent_home__` stuck on "Loading..." forever).
 *   - inflight requests abort on `invalidate*` and the resulting
 *     `TreeNode` transitions back to `idle` so the next fetch
 *     succeeds cleanly.
 *   - `abortAll` cancels every inflight fetch without dropping
 *     cached entries (workspace switch path).
 *   - within `freshMs` a second `fetch` is a cache hit; within
 *     `staleMs` a second `fetch` returns `stale` AND kicks a
 *     background revalidate; past `staleMs` it refetches.
 *   - HTTP / network errors surface as `kind:"error"` with a
 *     `TreeError` whose `cause` is exactly `http` | `network` | `abort`.
 *   - LRU evicts in insertion order when capacity is exceeded.
 *
 * Each test creates a fresh cache (no shared globals), making the
 * suite safe to run in any order and trivially parallelizable.
 */

import { describe, it, expect, vi } from "vitest";
import { createTreeCache } from "./treeCache";
import { treeKey, type TreeResponse } from "./types";

const k = (a = "a1", w = "ws1", p = "") => treeKey(a, w, p);

const fakeResp = (entries: { name: string; type: "file" | "directory" }[] = []): TreeResponse => ({
  root: "/ws",
  entries: entries.map((e) => ({ ...e })),
});

/** A fetcher whose resolution we control per-test (ignores AbortSignal). */
function deferredFetcher() {
  let resolveFn: ((r: TreeResponse) => void) | null = null;
  const fetcher = vi.fn((_signal: AbortSignal) => {
    return new Promise<TreeResponse>((resolve) => {
      resolveFn = resolve;
    });
  });
  return {
    fetcher,
    resolve: (r: TreeResponse) => {
      if (resolveFn) resolveFn(r);
      resolveFn = null;
    },
  };
}

/** A fetcher that throws when the signal aborts (mimics real fetch). */
function abortableFetcher() {
  const fetcher = vi.fn((signal: AbortSignal) =>
    new Promise<TreeResponse>((_resolve, reject) => {
      if (signal.aborted) {
        reject(new DOMException("aborted", "AbortError"));
        return;
      }
      signal.addEventListener("abort", () => {
        reject(new DOMException("aborted", "AbortError"));
      });
    }),
  );
  return fetcher;
}

describe("createTreeCache — dedup", () => {
  it("shares one in-flight Promise across concurrent callers", async () => {
    const cache = createTreeCache();
    const { fetcher, resolve } = deferredFetcher();

    const p1 = cache.fetch(k(), fetcher);
    const p2 = cache.fetch(k(), fetcher);
    const p3 = cache.fetch(k(), fetcher);

    expect(fetcher).toHaveBeenCalledTimes(1);
    resolve(fakeResp([{ name: "x", type: "file" }]));
    const [n1, n2, n3] = await Promise.all([p1, p2, p3]);
    expect(n1.kind).toBe("ready");
    expect(n2).toBe(n1);
    expect(n3).toBe(n1);
  });

  it("second call after a full clock-window fetch is a fresh refetch", async () => {
    // Inside freshMs, the second fetch is a cache hit (no HTTP).
    // We use an injected clock to jump past staleMs → refetch.
    let t = 1_000_000;
    const cache = createTreeCache({
      now: () => t,
      policy: { freshMs: 30_000, staleMs: 60_000, capacity: 100 },
    });
    const fetcher = vi.fn().mockResolvedValue(fakeResp());
    await cache.fetch(k(), fetcher);
    expect(fetcher).toHaveBeenCalledTimes(1);

    t += 120_000; // past staleMs
    await cache.fetch(k(), fetcher);
    expect(fetcher).toHaveBeenCalledTimes(2);
  });

  it("dedup key is per-key: different relPath → different in-flight entry", async () => {
    const cache = createTreeCache();
    const fetcher = vi.fn().mockResolvedValue(fakeResp());

    await Promise.all([
      cache.fetch(k("a1", "ws1", "src"), fetcher),
      cache.fetch(k("a1", "ws1", "docs"), fetcher),
    ]);
    expect(fetcher).toHaveBeenCalledTimes(2);
  });
});

describe("createTreeCache — cancellation", () => {
  it("invalidate aborts the in-flight fetch and the node transitions to idle", async () => {
    const cache = createTreeCache();
    const fetcher = abortableFetcher();

    const promise = cache.fetch(k(), fetcher);
    cache.invalidate(() => true);
    const node = await promise;
    // Aborted fetches revert to `idle` (NOT `error`) so the next
    // fetch starts clean without an explicit invalidate.
    expect(node.kind).toBe("idle");
    expect(cache.get(k()).kind).toBe("idle");
  });

  it("invalidateAgent only drops entries for the named agent", async () => {
    const cache = createTreeCache();
    const fetcher = vi.fn().mockResolvedValue(fakeResp());

    await cache.fetch(k("agent-a", "w1"), fetcher);
    await cache.fetch(k("agent-b", "w1"), fetcher);
    expect(cache.get(k("agent-a", "w1")).kind).toBe("ready");
    expect(cache.get(k("agent-b", "w1")).kind).toBe("ready");

    cache.invalidateAgent("agent-a");
    expect(cache.get(k("agent-a", "w1")).kind).toBe("idle");
    expect(cache.get(k("agent-b", "w1")).kind).toBe("ready");
  });

  it("abortAll cancels in-flight but keeps cached entries", async () => {
    const cache = createTreeCache();
    const fetcher = vi.fn().mockResolvedValue(fakeResp([{ name: "ok", type: "file" }]));
    await cache.fetch(k(), fetcher);
    expect(cache.get(k()).kind).toBe("ready");

    const slow = abortableFetcher();
    const pending = cache.fetch(k("a2", "w2"), slow);
    cache.abortAll();
    const node = await pending;
    expect(node.kind).toBe("idle");
    // The previously cached entry is NOT touched.
    expect(cache.get(k()).kind).toBe("ready");
  });
});

describe("createTreeCache — staleness / freshness", () => {
  it("within freshMs, second fetch is a no-op (no new HTTP call)", async () => {
    let t = 1_000_000;
    const cache = createTreeCache({
      now: () => t,
      policy: { freshMs: 30_000, staleMs: 5 * 60_000, capacity: 100 },
    });
    const fetcher = vi.fn().mockResolvedValue(fakeResp());
    await cache.fetch(k(), fetcher);

    t += 5_000;
    await cache.fetch(k(), fetcher);

    expect(fetcher).toHaveBeenCalledTimes(1);
  });

  it("within staleMs, second fetch returns `stale` AND kicks background revalidate", async () => {
    let t = 1_000_000;
    const cache = createTreeCache({
      now: () => t,
      policy: { freshMs: 30_000, staleMs: 5 * 60_000, capacity: 100 },
    });
    const fetcher = vi.fn().mockResolvedValue(fakeResp([{ name: "v1", type: "file" }]));
    await cache.fetch(k(), fetcher);
    expect(fetcher).toHaveBeenCalledTimes(1);

    t += 60_000;
    fetcher.mockResolvedValueOnce(fakeResp([{ name: "v2", type: "file" }]));
    const node = await cache.fetch(k(), fetcher);
    expect(node.kind).toBe("stale");
    // Background revalidate fires into the microtask queue; give it a tick.
    await new Promise((r) => setTimeout(r, 5));
    expect(fetcher.mock.calls.length).toBeGreaterThanOrEqual(2);
    expect(cache.get(k()).kind).toBe("ready");
  });

  it("past staleMs, second fetch refetches and returns ready", async () => {
    let t = 1_000_000;
    const cache = createTreeCache({
      now: () => t,
      policy: { freshMs: 30_000, staleMs: 5 * 60_000, capacity: 100 },
    });
    const fetcher = vi.fn().mockResolvedValue(fakeResp());
    await cache.fetch(k(), fetcher);

    t += 6 * 60_000; // past staleMs
    fetcher.mockResolvedValueOnce(fakeResp());
    const node = await cache.fetch(k(), fetcher);
    expect(node.kind).toBe("ready");
    expect(fetcher).toHaveBeenCalledTimes(2);
  });
});

describe("createTreeCache — error classification", () => {
  it("non-OK HTTP response surfaces as `kind:error` with `cause:http`", async () => {
    const cache = createTreeCache();
    const httpError = Object.assign(new Error("503"), {
      status: 503,
      statusText: "Service Unavailable",
    });
    const fetcher = vi.fn().mockRejectedValue(httpError);
    const node = await cache.fetch(k(), fetcher);
    expect(node.kind).toBe("error");
    if (node.kind === "error") {
      expect(node.error.cause).toBe("http");
      if (node.error.cause === "http") {
        expect(node.error.status).toBe(503);
      }
    }
  });

  it("non-Error throw is wrapped into `cause:network`", async () => {
    const cache = createTreeCache();
    const fetcher = vi.fn().mockRejectedValue("weird string failure");
    const node = await cache.fetch(k(), fetcher);
    expect(node.kind).toBe("error");
    if (node.kind === "error") expect(node.error.cause).toBe("network");
  });

  it("user-triggered abort via AbortController → `kind:idle` (NOT error)", async () => {
    const cache = createTreeCache();
    const fetcher = abortableFetcher();
    const promise = cache.fetch(k(), fetcher);
    cache.invalidate(() => true);
    const node = await promise;
    expect(node.kind).toBe("idle");
  });
});

describe("createTreeCache — pub/sub", () => {
  it("emits a `ready` transition on a successful fetch", async () => {
    const cache = createTreeCache();
    const listener = vi.fn();
    cache.subscribe(listener);
    const fetcher = vi.fn().mockResolvedValue(fakeResp([{ name: "x", type: "file" }]));
    await cache.fetch(k(), fetcher);
    const sawReady = listener.mock.calls.some(([, node]) => node.kind === "ready");
    expect(sawReady).toBe(true);
  });

  it("listener throwing does not break other subscribers or the cache", async () => {
    const cache = createTreeCache();
    cache.subscribe(() => {
      throw new Error("listener boom");
    });
    const good = vi.fn();
    cache.subscribe(good);
    const fetcher = vi.fn().mockResolvedValue(fakeResp());
    // Must not throw.
    await cache.fetch(k(), fetcher);
    expect(good).toHaveBeenCalled();
  });
});

describe("createTreeCache — LRU", () => {
  it("evicts in insertion order when capacity is exceeded", async () => {
    const cache = createTreeCache({
      policy: { capacity: 3, freshMs: 0, staleMs: 0 },
    });
    const fetcher = vi.fn().mockResolvedValue(fakeResp());
    await cache.fetch(k("a", "w", "1"), fetcher);
    await cache.fetch(k("a", "w", "2"), fetcher);
    await cache.fetch(k("a", "w", "3"), fetcher);
    await cache.fetch(k("a", "w", "4"), fetcher);

    expect(cache.get(k("a", "w", "1")).kind).toBe("idle");
    expect(cache.get(k("a", "w", "2")).kind).toBe("ready");
    expect(cache.get(k("a", "w", "3")).kind).toBe("ready");
    expect(cache.get(k("a", "w", "4")).kind).toBe("ready");
  });
});

describe("createTreeCache — StrictMode double-invoke regression", () => {
  // This is THE test that locks the fix for the production bug.
  // Two `fetch` calls issued in the same microtask for the same key
  // must produce exactly ONE HTTP request and resolve with the SAME
  // node — not with one node and a silent null.
  it("two immediate fetches share one request and one node", async () => {
    const cache = createTreeCache();
    const fetcher = vi.fn().mockResolvedValue(fakeResp([{ name: "x", type: "file" }]));
    const [n1, n2] = await Promise.all([
      cache.fetch(k(), fetcher),
      cache.fetch(k(), fetcher),
    ]);
    expect(fetcher).toHaveBeenCalledTimes(1);
    expect(n1.kind).toBe("ready");
    expect(n2.kind).toBe("ready");
    expect(n1).toBe(n2);
  });
});
