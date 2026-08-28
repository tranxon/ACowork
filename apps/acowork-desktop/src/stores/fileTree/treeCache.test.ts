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

/**
 * A fetcher whose behavior per call we fully control: each invocation
 * returns a fresh deferred promise whose resolve/reject we hold. Useful
 * for asserting ORDER of calls (e.g. foreground preempts background).
 */
function scriptedFetcher() {
  const calls: Array<{
    resolve: (r: TreeResponse) => void;
    reject: (e: unknown) => void;
    signal: AbortSignal;
  }> = [];
  const fetcher = vi.fn((signal: AbortSignal) => {
    const p = new Promise<TreeResponse>((resolve, reject) => {
      // Mimic real fetch: an aborted signal rejects the in-flight
      // request. Without this, `abortAll` tests would hang on the
      // deferred promise.
      if (signal.aborted) {
        reject(new DOMException("aborted", "AbortError"));
        return;
      }
      signal.addEventListener("abort", () => {
        reject(new DOMException("aborted", "AbortError"));
      });
      calls.push({
        resolve: (r) => resolve(r),
        reject: (e) => reject(e),
        signal,
      });
    });
    return p;
  });
  return { fetcher, calls };
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

  it("abortAll(exceptAgentId) keeps the named agent's in-flight fetches alive", async () => {
    const cache = createTreeCache();
    const { fetcher: fetcherA, calls: callsA } = scriptedFetcher();
    const { fetcher: fetcherB, calls: callsB } = scriptedFetcher();
    const pendingA = cache.fetch(k("agent-a", "w1"), fetcherA);
    const pendingB = cache.fetch(k("agent-b", "w1"), fetcherB);

    cache.abortAll("agent-a");

    // agent-b was cancelled: its signal is aborted and the node
    // reverts to `idle` (NOT `error`).
    expect(callsB[0].signal.aborted).toBe(true);
    expect((await pendingB).kind).toBe("idle");

    // agent-a survived: its signal is NOT aborted; resolve the fetcher
    // and the fetch completes to `ready`.
    expect(callsA[0].signal.aborted).toBe(false);
    callsA[0].resolve(fakeResp([{ name: "x", type: "file" }]));
    expect((await pendingA).kind).toBe("ready");
    expect(cache.get(k("agent-a", "w1")).kind).toBe("ready");
    expect(cache.get(k("agent-b", "w1")).kind).toBe("idle");
  });

  it("abortAll(exceptAgentId) never matches a different agent's keys (\\u0000 seal)", async () => {
    const cache = createTreeCache();
    const { fetcher, calls } = scriptedFetcher();
    const pending = cache.fetch(k("agent-aa", "w1"), fetcher);

    // The except-prefix is sealed by `\u0000`, so `agent-a` can never
    // protect `agent-aa`'s keys — a bare prefix check would.
    cache.abortAll("agent-a");

    expect(calls[0].signal.aborted).toBe(true);
    expect((await pending).kind).toBe("idle");
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

describe("createTreeCache — invalidate + fetch (authoritative refresh)", () => {
  // Regression lock for the ea819127 follow-up: after a successful
  // server-side mutation (create / rename / delete / paste), the UI
  // must converge immediately. SWR's fresh-hit fast path otherwise keeps
  // serving pre-mutation entries within `freshMs`, and the next user
  // action against the now-stale `relPath` will 404.
  it("invalidate-then-fetch drops the fresh entry and re-fetches synchronously", async () => {
    let t = 1_000_000;
    const cache = createTreeCache({ now: () => t });
    const fetcher = vi.fn().mockResolvedValue(fakeResp([{ name: "old", type: "file" }]));
    await cache.fetch(k(), fetcher); // warm cache, fetchedAt = t
    expect(fetcher).toHaveBeenCalledTimes(1);

    // Mutate on the server: a brand-new entry appears. Without
    // invalidate, the next `fetch` is a fresh hit and the new entry is
    // invisible.
    t += 100; // still well within freshMs
    const stillFresh = await cache.fetch(k(), fetcher);
    expect(stillFresh.kind).toBe("ready");
    expect(fetcher).toHaveBeenCalledTimes(1); // SWR fast-path served the stale entry

    // Authoritative refresh path: drop the entry, then fetch again.
    cache.invalidate((key) => key === k());
    expect(cache.get(k()).kind).toBe("idle");

    fetcher.mockResolvedValueOnce(fakeResp([{ name: "new", type: "file" }]));
    const refreshed = await cache.fetch(k(), fetcher);
    expect(refreshed.kind).toBe("ready");
    if (refreshed.kind === "ready") {
      expect(refreshed.entries[0].name).toBe("new");
    }
    expect(fetcher).toHaveBeenCalledTimes(2);
  });

  it("invalidate aborts an inflight fetch started before the mutation", async () => {
    const cache = createTreeCache();
    const df = deferredFetcher();
    // Kick off an in-flight fetch for the pre-mutation state.
    const p1 = cache.fetch(k(), df.fetcher);
    expect(cache.get(k()).kind).toBe("loading");

    // Mutation succeeds on the server before the previous fetch returns.
    // Invalidate-then-refresh must abort the old fetch so its late
    // resolution cannot race the new state.
    cache.invalidate((key) => key === k());
    expect(cache.get(k()).kind).toBe("idle");

    // Resolve the OLD fetch AFTER the invalidate. Because the abort
    // fired, doFetch's body returns `{kind:"idle"}` instead of writing
    // back the stale response.
    df.resolve(fakeResp([{ name: "stale", type: "file" }]));
    const abandoned = await p1;
    expect(abandoned.kind).toBe("idle");
  });
});

describe("createTreeCache — SWR background revalidation (RFC 5861)", () => {
  // Regression lock for the follow-up review finding: SWR's background
  // revalidation must be INVISIBLE to the UI.
  //   1. Failure must NOT overwrite the cached entry — the stale data
  //      the user can already see keeps working (no error flash).
  //   2. It must never enter `kind:"loading"` — otherwise every stale
  //      revalidation would blank the tree (Loading... flicker).
  //   3. A foreground fetch for the same key preempts (aborts) the
  //      background one so the authoritative result wins.

  it("failure during background revalidate preserves the stale entry (no error flash)", async () => {
    let t = 1_000_000;
    const cache = createTreeCache({
      now: () => t,
      policy: { freshMs: 30_000, staleMs: 5 * 60_000, capacity: 100 },
    });
    const fetcher = vi
      .fn()
      .mockResolvedValueOnce(fakeResp([{ name: "v1", type: "file" }]));
    await cache.fetch(k(), fetcher);
    expect(cache.get(k()).kind).toBe("ready");

    // Move into the stale window and let the revalidate fail.
    t += 60_000;
    fetcher.mockRejectedValueOnce(Object.assign(new Error("503"), { status: 503 }));
    const node = await cache.fetch(k(), fetcher);
    expect(node.kind).toBe("stale");

    // Give the background fetch a microtask to run and fail.
    await new Promise((r) => setTimeout(r, 5));
    const after = cache.get(k());
    expect(after.kind).toBe("ready"); // still the OLD entries, NOT error
    if (after.kind === "ready") {
      expect(after.entries[0].name).toBe("v1");
    }
  });

  it("background revalidate never enters `loading` (no flicker)", async () => {
    let t = 1_000_000;
    const cache = createTreeCache({
      now: () => t,
      policy: { freshMs: 30_000, staleMs: 5 * 60_000, capacity: 100 },
    });
    const states: string[] = [];
    cache.subscribe((key, node) => {
      if (key === k()) states.push(node.kind);
    });
    const fetcher = vi
      .fn()
      .mockResolvedValueOnce(fakeResp([{ name: "v1", type: "file" }]));
    await cache.fetch(k(), fetcher);
    states.length = 0; // clear the initial loading→ready transitions

    t += 60_000;
    const node = await cache.fetch(k(), fetcher); // stale hit → background
    expect(node.kind).toBe("stale");
    await new Promise((r) => setTimeout(r, 5));

    // The ONLY state transitions after the initial load are stale→ready.
    // A `loading` in here would mean the tree blanked during revalidate.
    expect(states).not.toContain("loading");
    expect(cache.get(k()).kind).toBe("ready");
  });

  it("successful background revalidate overwrites the stale entry", async () => {
    let t = 1_000_000;
    const cache = createTreeCache({
      now: () => t,
      policy: { freshMs: 30_000, staleMs: 5 * 60_000, capacity: 100 },
    });
    const fetcher = vi
      .fn()
      .mockResolvedValueOnce(fakeResp([{ name: "v1", type: "file" }]));
    await cache.fetch(k(), fetcher);

    t += 60_000;
    fetcher.mockResolvedValueOnce(fakeResp([{ name: "v2", type: "file" }]));
    const node = await cache.fetch(k(), fetcher);
    expect(node.kind).toBe("stale");
    await new Promise((r) => setTimeout(r, 5));
    const after = cache.get(k());
    expect(after.kind).toBe("ready");
    if (after.kind === "ready") {
      expect(after.entries[0].name).toBe("v2");
    }
  });

  it("foreground fetch preempts (aborts) an in-flight background revalidate", async () => {
    let t = 1_000_000;
    const cache = createTreeCache({
      now: () => t,
      policy: { freshMs: 30_000, staleMs: 5 * 60_000, capacity: 100 },
    });
    const sf = scriptedFetcher();
    // First call: initial load. (scriptedFetcher defers resolution, so
    // start the fetch, resolve call #0, then await.)
    const initial = cache.fetch(k(), sf.fetcher);
    sf.calls[0].resolve(fakeResp([{ name: "v1", type: "file" }]));
    await initial;
    expect(cache.get(k()).kind).toBe("ready");

    // Second call: stale hit → background revalidate starts (call #2).
    t += 60_000;
    const stale = await cache.fetch(k(), sf.fetcher);
    expect(stale.kind).toBe("stale");
    expect(sf.calls.length).toBe(2);

    // Third call: a FOREGROUND fetch (e.g. user refresh) — must abort
    // the in-flight background request #2. Note: the cache entry is
    // still `ready` (background never enters loading), so this goes
    // through the stale branch again; doBackgroundFetch must preempt.
    // To force the foreground path we jump past staleMs first.
    t += 5 * 60_000;
    const fg = cache.fetch(k(), sf.fetcher);
    expect(sf.calls.length).toBe(3);
    // The background request's signal is aborted.
    expect(sf.calls[1].signal.aborted).toBe(true);

    // Resolve the foreground with the authoritative data.
    sf.calls[2].resolve(fakeResp([{ name: "v3", type: "file" }]));
    const fgNode = await fg;
    expect(fgNode.kind).toBe("ready");
    if (fgNode.kind === "ready") {
      expect(fgNode.entries[0].name).toBe("v3");
    }
    expect(cache.get(k()).kind).toBe("ready");
  });

  it("background revalidate does not join a foreground fetch already in flight", async () => {
    let t = 1_000_000;
    const cache = createTreeCache({
      now: () => t,
      policy: { freshMs: 30_000, staleMs: 5 * 60_000, capacity: 100 },
    });
    const sf = scriptedFetcher();
    // Initial load.
    const initial = cache.fetch(k(), sf.fetcher);
    sf.calls[0].resolve(fakeResp([{ name: "v1", type: "file" }]));
    await initial;

    // Foreground fetch starts (call #2) and hangs.
    t += 6 * 60_000; // past staleMs so the foreground path is taken
    const fgPromise = cache.fetch(k(), sf.fetcher);
    expect(sf.calls.length).toBe(2);

    // The cache entry is now `loading` (foreground path), so another
    // fetch joins the in-flight Promise — it must NOT spawn a third
    // fetch even though it would otherwise hit the stale branch.
    const joined = cache.fetch(k(), sf.fetcher);
    expect(sf.calls.length).toBe(2);

    sf.calls[1].resolve(fakeResp([{ name: "v2", type: "file" }]));
    const [node, node2] = await Promise.all([fgPromise, joined]);
    expect(node.kind).toBe("ready");
    expect(node2).toBe(node);
    expect(sf.calls.length).toBe(2);
  });
});
