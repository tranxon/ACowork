/**
 * fileTree — Zustand adapter
 *
 * Bridges the framework-agnostic `TreeCache` to React. The cache is the
 * authoritative store; this Zustand wrapper exists only to (a) mirror
 * cache nodes into a shallow `Record<key, node>` so React selectors can
 * subscribe per-key, and (b) expose action helpers that compose
 * `cache.fetch(key, fetcher(...))` into the friendly
 * `fetch(agentId, workspaceId, relPath)` signature the UI uses.
 *
 * Lifecycle:
 *   - The cache + client are created once at module load. The store
 *     re-emits cache transitions to `nodes` (so React re-renders on
 *     loading→ready transitions, not on every listener call).
 *   - `invalidate(agentId)` is called from WS reconnect (full sync)
 *     and from `workspaceFsEvents.refreshTreesForChanges` callers that
 *     want to drop all tree state for an agent.
 *   - `abortAll(agentId)` is called from `agentStore.selectAgent` so an
 *     old agent's in-flight fetches can't race the new selection (keys
 *     are per-agent so there's no cache contamination — this is purely
 *     a bandwidth / freshness optimization). The newly-selected agent's
 *     own in-flight fetches are KEPT alive: aborting them would leave
 *     its tree stuck on `idle` with no re-fetch scheduled (the FileTree
 *     mount effect re-fetches idle roots, but only on state change —
 *     an abort mid-flight after mount would strand the UI on
 *     "Loading…" forever).
 *
 * Why two layers: keeping the cache framework-free means we can unit
 * test dedup/abort/SWR with zero React/Zustand mocks; the Zustand
 * adapter then becomes a thin (and trivially correct) glue layer.
 */

import { create } from "zustand";
import { log } from "../../lib/logger";
import { createTreeClient } from "./treeClient";
import { createTreeCache, type TreeCache } from "./treeCache";
import { getGatewayUrl } from "../../lib/config";
import { treeKey, type TreeCacheKey, type TreeNode } from "./types";

interface FileTreeState {
  /** Per-key mirror of the cache's authoritative TreeNode state. */
  nodes: Record<string, TreeNode>;
  /**
   * Kick off (or join) a fetch for one directory. Resolves with the
   * terminal node (`ready` | `stale` | `error` | `idle`). Callers
   * usually fire-and-forget; await only when they need the result.
   *
   * **SWR semantics apply** — a fresh cache entry (within `policy.freshMs`)
   * is served without a network round-trip. Use this for passive
   * updates (fs-changed events, auto-refresh timers, etc.).
   *
   * For *authoritative* refreshes after a mutation that just succeeded
   * on the server (create / rename / move / delete / paste), call
   * [`refresh`](Self::refresh) instead — otherwise the UI will keep
   * showing the pre-mutation tree and the next action will race against
   * stale state (issue: ea819127 introduced this regression).
   */
  fetch: (agentId: string, workspaceId: string, relPath?: string) => Promise<TreeNode>;
  /**
   * Force a fresh fetch for one directory, **bypassing SWR**. Use this
   * after a successful server-side mutation (create / rename / move /
   * delete / paste) so the UI immediately reflects the new state.
   *
   * The cache key for the target directory is dropped and the fetch is
   * awaited to its terminal state. Errors are surfaced to the caller
   * (same shape as `fetch`); the previous `ready` node is NOT restored
   * on error — the caller should treat a failure as "tree is now
   * unknown; the next read will start a background revalidation".
   *
   * Contract for mutation callers (WorkspaceExplorer / paste handler):
   * check the returned `kind` — on `"error"` the directory was NOT
   * re-fetched, so any `requestRenameFor(...)` targeting a node inside
   * it will silently fail to render. Surface a toast and bail out
   * instead of entering rename mode.
   */
  refresh: (agentId: string, workspaceId: string, relPath?: string) => Promise<TreeNode>;
  /** Drop everything belonging to one agent. Aborts in-flight fetches. */
  invalidate: (agentId: string) => void;
  /** Drop everything. Use on logout / hard reset. */
  clear: () => void;
  /** Cancel all in-flight fetches without dropping cache (agent switch).
   *  Pass the newly-selected agent so ITS tree keeps loading. */
  abortAll: (exceptAgentId?: string) => void;
  /** Read the current node for a key. Useful from non-React callers. */
  getNode: (key: TreeCacheKey) => TreeNode;
}

const cache: TreeCache = createTreeCache();
const fetchFor = createTreeClient({ gatewayUrl: getGatewayUrl });

cache.subscribe((key, node) => {
  // Drop `idle` mirrors — they would only add noise to the mirror
  // and break "is this cached?" checks (`nodes[key]` truthiness is
  // the canonical "do we have data" check from React components).
  useFileTreeStore.setState((s) => {
    const next = { ...s.nodes };
    if (node.kind === "idle") {
      delete next[key];
    } else {
      next[key] = node;
    }
    return { nodes: next };
  });
});

export const useFileTreeStore = create<FileTreeState>((set) => ({
  nodes: {},

  async fetch(agentId, workspaceId, relPath) {
    const key = treeKey(agentId, workspaceId, relPath ?? "");
    const node = await cache.fetch(key, (signal) => fetchFor(agentId, workspaceId, relPath ?? "")(signal));
    return node;
  },

  /**
   * Authoritative refresh for one directory after a successful server
   mutation. Drops the SWR entry and re-fetches synchronously. See the
   interface comment for the rationale and contract.
   */
  async refresh(agentId, workspaceId, relPath) {
    const path = relPath ?? "";
    const key = treeKey(agentId, workspaceId, path);
    // Abort any in-flight fetch for this exact key (so the SWR fast-path
    // can't race a mutation that's mid-flight on the server) and drop
    // the cached entry. After this point `cache.fetch(key, ...)` will
    // always go through `doFetch` regardless of TTL.
    cache.invalidate((k) => k === key);
    return cache.fetch(key, (signal) => fetchFor(agentId, workspaceId, path)(signal));
  },

  invalidate(agentId) {
    cache.invalidateAgent(agentId);
  },

  clear() {
    cache.clear();
    set({ nodes: {} });
  },

  abortAll(exceptAgentId?: string) {
    cache.abortAll(exceptAgentId);
  },

  getNode(key) {
    return cache.get(key);
  },
}));

/**
 * Escape hatch for tests: write a TreeNode directly into the cache,
 * bypassing fetch. Production code MUST go through `fetch()` so that
 * dedup / abort / SWR semantics apply. Exported as a named export so
 * the temptation to call it from production UI is visible at the
 * call site.
 *
 * @internal test-only
 */
export function __seedTreeNode(key: TreeCacheKey, node: TreeNode): void {
  cache.set(key, node);
}

/**
 * Imperative helper for non-React callers (fs event listener, etc.).
 * Equivalent to `useFileTreeStore.getState().fetch(...)`.
 */
export async function fetchTreeNode(
  agentId: string,
  workspaceId: string,
  relPath?: string,
): Promise<TreeNode> {
  try {
    return await useFileTreeStore.getState().fetch(agentId, workspaceId, relPath);
  } catch (e) {
    log.error("[FileTreeStore] fetch failed:", e);
    return { kind: "error", error: { cause: "network", message: String(e) } };
  }
}

/**
 * Imperative helper for non-React callers (mutation handlers, paste /
,
 * rename post-actions, etc.) that need to force a fresh fetch. Equivalent
 * to `useFileTreeStore.getState().refresh(...)`.
 */
export async function refreshTreeNode(
  agentId: string,
  workspaceId: string,
  relPath?: string,
): Promise<TreeNode> {
  try {
    return await useFileTreeStore.getState().refresh(agentId, workspaceId, relPath);
  } catch (e) {
    log.error("[FileTreeStore] refresh failed:", e);
    return { kind: "error", error: { cause: "network", message: String(e) } };
  }
}
