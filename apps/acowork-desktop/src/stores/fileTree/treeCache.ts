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

  /** Abort every in-flight fetch without dropping entries. Useful on workspace switch. */
  abortAll(): void;

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
  const inflight = new Map<TreeCacheKey, { promise: Promise<TreeNode>; abort: AbortController }>();
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
    // If a fetch is already in flight for this key, just join it.
    const live = inflight.get(key);
    if (live) return live.promise;

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
    inflight.set(key, { promise, abort });
    return promise;
  };

  /** Background revalidation; fire-and-forget. Used for SWR. */
  const revalidateInBackground = (key: TreeCacheKey, fetcher: TreeFetcher): void => {
    if (inflight.has(key)) return;
    void doFetch(key, fetcher);
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

  const abortAll = (): void => {
    for (const { abort } of Array.from(inflight.values())) {
      abort.abort();
    }
    inflight.clear();
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
        if (t - existing.fetchedAt < policy.staleMs) {
          revalidateInBackground(key, fetcher);
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
