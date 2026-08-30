/**
 * fileTree — read-side helpers
 *
 * Imperative lookups for the cached workspace root path. Extracted
 * because (a) three call sites used to read `treeRoots[agentId:workspaceId]`
 * directly from the workspace store, and (b) React-rendered code can
 * subscribe via the cache, but non-React code (event handlers, tool
 * callbacks) just needs a synchronous value.
 */

import { useFileTreeStore } from "./fileTreeStore";
import { treeKey, isReadyNode } from "./types";

/**
 * Resolve the absolute filesystem root for an agent+workspace, or an
 * empty string if the workspace tree has not been fetched yet.
 *
 * Use the imperative form (e.g. inside `addAttachedContext`) when
 * you cannot subscribe to a Zustand selector. For rendered components,
 * prefer `useFileTreeStore(s => s.nodes[workspaceRootKey(a,w)]?.root)`
 * so the component re-renders when the root becomes available.
 */
export function getCachedWorkspaceRoot(agentId: string, workspaceId: string): string {
  const node = useFileTreeStore.getState().getNode(treeKey(agentId, workspaceId, ""));
  return isReadyNode(node) ? node.root : "";
}
