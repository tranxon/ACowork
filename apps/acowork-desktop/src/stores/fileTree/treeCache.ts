/**
 * fileTree — core cache
 *
 * Pure, framework-agnostic cache for workspace tree nodes. Owns:
 *   - node storage (LRU, capacity-bounded)
 *   - dedup of concurrent fetches per key (Promise sharing)
 *   - cancellation (AbortController per inflight, `abortAll` for
 *     workspace switches, `invalidate*` cleans up)
 *   - stale-while-revalidate semantics driven by TreeCachePolicy
 *   - pub/sub so the Zustand adapter can mirror changes
 *
 * Why a separate module: the previous inlined implementation used a
 * module-level `inflightFetches` Map that lived outside the Zustand
 * store, which made it (1) un-testable without globals, (2)
 * un-resettable, (3) impossible to enumerate for the invalidate path
 * (the bug we are fixing was caused by `invalidateTreeCache` forgetting
 * to clean it). This cache instance is created once per FileTree store
 * and is the only authoritative owner of in-flight state.
 *
 * The cache is **transport-agnostic**: callers inject a `fetcher`
 * function. Tests inject a fake; production wires a real fetch with
 * AbortSignal. No `fetch()`, `import.meta.env`, or `URL` lives here.
 */

import type {
  TreeCacheKey,
  TreeCachePolicy,
  TreeError,
  TreeNode,
  TreeResponse,
} from "./types";
import { DEFAULT_TREE_POLICY } from "./types";

export type TreeFetcher = (signal: AbortSignal) => Promise<TreeResponse>;

export type Listener = (key: TreeCacheKey, node: TreeNode) => void;

export interface TreeCache {
  /** Synchronous read. Returns `kind:"idle"` if unknown. */
  get(key: TreeCacheKey): TreeNode;

  /**
   * Imperative write — used by tests to seed the cache and by
   * `fileTreeStore` to install the initial entries. Production code
   * should go through `fetch()` so dedup / abort / SWR semantics
   * apply; this method is for fixtures and replay scenarios only.
   */
  set(key: TreeCacheKey, node: TreeNode): void;

  /**
   * Fetch or revalidate a node. Returns the eventual TreeNode:
   *   - fresh cached → returns `ready` immediately, no fetch
   *   - stale cached → returns `stale` immediately, kicks background revalidate
   *     (the background fetch never enters `loading` and silently
   *      preserves the stale entry on failure — RFC 5861 §3)
   *   - dedup hit    → joins the existing in-flight Promise
   *   - otherwise    → marks `loading`, fetches, resolves to `ready` | `error` | `idle`
   *
   * Calling this from an unmounting component is safe: the returned
   * Promise resolves to `kind:"idle"` if the abort fires.
   */
  fetch(key: TreeCacheKey, fetcher: TreeFetcher): Promise<TreeNode>;

  /** Drop entries that match. Aborts any in-flight fetches for them. */
  invalidate(matcher: (key: TreeCacheKey) => boolean): void;

  /** Convenience: drop every entry whose key starts with `${agentId}\u0000`. */
  invalidateAgent(agentId: string): void;

  /** Drop all entries (e.g. logout). Aborts everything in flight. */
  clear(): void;

  /**
   * Abort in-flight fetches without dropping cached entries.
   *
   * Pass `exceptAgentId` to keep that agent's in-flight fetches alive
   * (agent-switch path: the newly-selected agent's tree must keep
   * loading while every other agent's requests are cancelled — a
   * blanket abort would leave the visible tree stuck on `idle` with
   * no component re-fetching it, see the FileTree mount-effect fix).
   */
  abortAll(exceptAgentId?: string): void;

  /** Subscribe to node transitions. Returned fn unsubscribes. */
  subscribe(listener: Listener): () => void;
}

/* ─────────────────────────── LRU (tiny, inlined) ────────────────── */

/**
 * Tiny LRU. Avoids depending on `lru-cache` (an npm package): the
 * desktop bundle already has plenty, and the LRU needs are bounded
 * (~500 keys). Implementing it here keeps `treeCache.ts` test-friendly
 * with zero transitive dependencies and makes the eviction order
 * obvious when reading the file.
 */
class LRU<K, V> {
  private readonly cap: number;
  private readonly map = new Map<K, V>();

  constructor(capacity: number) {
    this.cap = Math.max(1, capacity);
  }

  get(key: K): V | undefined {
    const v = this.map.get(key);
    if (v === undefined) return undefined;
    this.map.delete(key);
    this.map.set(key, v);
    return v;
  }

  set(key: K, value: V): void {
    if (this.map.has(key)) this.map.delete(key);
    this.map.set(key, value);
    while (this.map.size > this.cap) {
      const oldest = this.map.keys().next().value;
      if (oldest === undefined) break;
      this.map.delete(oldest);
    }
  }

  delete(key: K): boolean {
    return this.map.delete(key);
  }

  has(key: K): boolean {
    return this.map.has(key);
  }

  keys(): IterableIterator<K> {
    return this.map.keys();
  }

  clear(): void {
    this.map.clear();
  }

  get size(): number {
    return this.map.size;
  }
}

/* ─────────────────────────── factory ─────────────────────────────── */

/**
 * Create a fresh cache instance. Tests create one per test for
 * isolation; production creates exactly one (in fileTreeStore).
 */
export function createTreeCache(opts?: {
  policy?: Partial<TreeCachePolicy>;
  /** Injectable `now()` — tests override to make TTL deterministic. */
  now?: () => number;
}): TreeCache {
  const policy: TreeCachePolicy = { ...DEFAULT_TREE_POLICY, ...opts?.policy };
  const now = opts?.now ?? (() => Date.now());

  const nodes = new LRU<TreeCacheKey, TreeNode>(policy.capacity);
  /**
   * In-flight fetches, tracked by cache key. The `background` flag
   * distinguishes SWR revalidations (must NOT clobber the UI-visible
   * state on failure — RFC 5861 §3) from foreground fetches (must
   * transition through `loading` and surface errors). Foreground
   * fetches abort any in-flight background revalidation for the same
   * key before starting so the user's authoritative request wins.
   */
  const inflight = new Map<
    TreeCacheKey,
    { promise: Promise<TreeNode>; abort: AbortController; background: boolean }
  >();
  const listeners = new Set<Listener>();

  const setNode = (key: TreeCacheKey, node: TreeNode) => {
    nodes.set(key, node);
    for (const l of listeners) {
      try {
        l(key, node);
      } catch (e) {
        // eslint-disable-next-line no-console
        console.error("[treeCache] listener threw:", e);
      }
    }
  };

  /** Map HTTP/network failure to our TreeError discriminated union. */
  const classifyError = (e: unknown, signal: AbortSignal): TreeError => {
    if (signal.aborted) return { cause: "abort" };
    if (
      e &&
      typeof e === "object" &&
      "status" in e &&
      typeof (e as { status: unknown }).status === "number"
    ) {
      const ex = e as { status: number; statusText?: string };
      return { cause: "http", status: ex.status, statusText: ex.statusText ?? "" };
    }
    if (e instanceof Error) return { cause: "network", message: e.message };
    return { cause: "network", message: String(e) };
  };

  const doFetch = async (
    key: TreeCacheKey,
    fetcher: TreeFetcher,
  ): Promise<TreeNode> => {
    // Foreground fetches preempt any in-flight background revalidation
    // for the same key — the user's authoritative request must win.
    // Background→background dedup is handled inside doBackgroundFetch
    // below, so we only need to consider the background case here.
    const live = inflight.get(key);
    if (live?.background) {
      live.abort.abort();
      inflight.delete(key);
    } else if (live) {
      // Concurrent foreground caller — join the existing Promise.
      return live.promise;
    }

    const abort = new AbortController();
    setNode(key, { kind: "loading" });
    const promise = (async () => {
      try {
        const data = await fetcher(abort.signal);
        if (abort.signal.aborted) {
          // The caller (e.g. invalidate) won the race. Don't write back.
          return { kind: "idle" as const };
        }
        const node: TreeNode = {
          kind: "ready",
          entries: data.entries,
          root: data.root,
          fetchedAt: now(),
        };
        setNode(key, node);
        return node;
      } catch (e) {
        const err = classifyError(e, abort.signal);
        // Aborted fetches are NOT errors from the caller's perspective:
        // the caller asked us to stop, so revert to `idle` so a future
        // fetch can succeed cleanly without first having to invalidate.
        const node: TreeNode =
          err.cause === "abort" ? { kind: "idle" } : { kind: "error", error: err };
        setNode(key, node);
        return node;
      } finally {
        inflight.delete(key);
      }
    })();
    inflight.set(key, { promise, abort, background: false });
    return promise;
  };

  /**
   * SWR background revalidation (RFC 5861 §3). The cache entry — if
   * one exists — MUST be preserved across this fetch:
   *
   *   - Loading: NOT entered. The UI keeps showing whatever it had
   *     (`ready` or `stale`); users must not see "Loading..." flicker
   *     because some bookkeeping code thought it was time to refresh.
   *   - Success: overwrite with the fresh entry.
   *   - Failure (including AbortError): silently drop. Stale data
   *     continues to be served; the next explicit foreground `fetch`
   *     will surface the error if the connection is still down.
   *
   * Dedup: if a foreground fetch is already in flight for the same
   * key, skip — the foreground fetch will write the cache on success.
   * Otherwise a second background revalidate is also skipped (it's
   * already running).
   */
  const doBackgroundFetch = (key: TreeCacheKey, fetcher: TreeFetcher): void => {
    if (inflight.has(key)) return;
    const abort = new AbortController();
    const promise = (async (): Promise<TreeNode> => {
      try {
        const data = await fetcher(abort.signal);
        if (abort.signal.aborted) return { kind: "idle" };
        const node: TreeNode = {
          kind: "ready",
          entries: data.entries,
          root: data.root,
          fetchedAt: now(),
        };
        // Only write if no foreground fetch started in the meantime —
        // a foreground request always takes priority over a stale
        // background result.
        if (!inflight.has(key) || inflight.get(key)?.background === true) {
          setNode(key, node);
        }
        return node;
      } catch {
        // SWR invariant: background revalidation failure does NOT
        // overwrite the cached entry. Stale data continues to be
        // served. The next foreground fetch (e.g. after staleMs
        // expires) will surface the underlying error to the UI.
        return { kind: "idle" };
      } finally {
        // Only delete if WE are still the in-flight entry — a
        // foreground fetch that pre-empted us would have replaced
        // the inflight slot by now.
        const live = inflight.get(key);
        if (live?.abort === abort) inflight.delete(key);
      }
    })();
    inflight.set(key, { promise, abort, background: true });
  };

  /** Drop matching entries + abort their inflight fetches. */
  const invalidate = (matcher: (key: TreeCacheKey) => boolean): void => {
    for (const k of Array.from(nodes.keys())) {
      if (!matcher(k)) continue;
      nodes.delete(k);
      const inf = inflight.get(k);
      if (inf) {
        inf.abort.abort();
        inflight.delete(k);
      }
      // Notify so subscribers drop their mirror; the next read returns idle.
      setNode(k, { kind: "idle" });
    }
  };

  const abortAll = (exceptAgentId?: string): void => {
    // Keys are sealed with `\u0000` (see treeKey), so a string prefix
    // is an exact agent filter — no false matches across agents.
    const keepPrefix = exceptAgentId ? `${exceptAgentId}\u0000` : null;
    for (const [key, { abort }] of Array.from(inflight.entries())) {
      if (keepPrefix && key.startsWith(keepPrefix)) continue;
      abort.abort();
      inflight.delete(key);
    }
  };

  return {
    get(key) {
      return nodes.get(key) ?? { kind: "idle" };
    },

    set(key, node) {
      setNode(key, node);
    },

    async fetch(key, fetcher) {
      const existing = nodes.get(key);
      const t = now();

      // 1. Fresh hit — no work.
      if (existing?.kind === "ready") {
        if (t - existing.fetchedAt < policy.freshMs) return existing;
        // 2. Stale hit — serve stale immediately, revalidate in background.
        //    The background fetch is silent on failure (preserves the
        //    stale entry) and on success overwrites with a fresh one.
        if (t - existing.fetchedAt < policy.staleMs) {
          doBackgroundFetch(key, fetcher);
          return { ...existing, kind: "stale" as const };
        }
      }

      return doFetch(key, fetcher);
    },

    invalidate,

    invalidateAgent(agentId) {
      // Brand-sealed keys: prefix is `${agentId}\u0000`. `treeKey()`
      // guarantees NUL cannot appear inside the agentId, so a string
      // prefix check is exact (no false matches across agents).
      const prefix = `${agentId}\u0000`;
      invalidate((k) => k.startsWith(prefix));
    },

    clear() {
      abortAll();
      nodes.clear();
    },

    abortAll,

    subscribe(listener) {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
  };
}
