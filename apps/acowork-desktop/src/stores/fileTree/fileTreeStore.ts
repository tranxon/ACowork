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
 *   - `abortAll()` is called on workspace switch so a stale workspace's
 *     fetches can't race the new one.
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
   */
  fetch: (agentId: string, workspaceId: string, relPath?: string) => Promise<TreeNode>;
  /** Drop everything belonging to one agent. Aborts in-flight fetches. */
  invalidate: (agentId: string) => void;
  /** Drop everything. Use on logout / hard reset. */
  clear: () => void;
  /** Cancel all in-flight fetches without dropping cache (workspace switch). */
  abortAll: () => void;
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

  invalidate(agentId) {
    cache.invalidateAgent(agentId);
  },

  clear() {
    cache.clear();
    set({ nodes: {} });
  },

  abortAll() {
    cache.abortAll();
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
