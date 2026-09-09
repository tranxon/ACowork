/**
 * ADR-073 §4: pure helper that partitions an agent list into per-node
 * groups for the remote-mode sidebar view. Kept side-effect-free so it
 * is trivially unit-testable without mocking stores.
 *
 * Contract:
 *   - `agents` are partitioned by `agent.node_id` (empty/missing →
 *     bucket key `"__unknown__"`).
 *   - Groups are emitted in the order they appear in `nodes` (the
 *     Gateway's natural ordering); empty groups are skipped.
 *   - Agents whose `node_id` is not present in `nodes` (or whose
 *     `node_id` is missing) are appended as a trailing "unknown"
 *     bucket so they never silently disappear from the UI.
 *   - Within each group the agents keep their input order (which the
 *     caller controls via `filteredAgents`).
 */
import type { AgentInfo, NodeInfo } from "../../lib/types";

export interface AgentNodeGroup {
  /** Stable bucket key — `node.node_id` for known nodes, `"__unknown__"` for the fallback. */
  nodeId: string;
  /** The matching NodeInfo, or `null` for the unknown bucket. */
  node: NodeInfo | null;
  /** Agents in this bucket, in their original order. */
  agents: AgentInfo[];
}

export const UNKNOWN_NODE_ID = "__unknown__";

export function partitionAgentsByNode(
  agents: AgentInfo[],
  nodes: NodeInfo[],
): AgentNodeGroup[] {
  const byNode = new Map<string, AgentInfo[]>();
  for (const a of agents) {
    const nid = a.node_id ?? UNKNOWN_NODE_ID;
    let arr = byNode.get(nid);
    if (!arr) {
      arr = [];
      byNode.set(nid, arr);
    }
    arr.push(a);
  }
  const ordered: AgentNodeGroup[] = [];
  for (const n of nodes) {
    const agentsForNode = byNode.get(n.node_id);
    if (agentsForNode && agentsForNode.length > 0) {
      ordered.push({ nodeId: n.node_id, node: n, agents: agentsForNode });
    }
  }
  const knownIds = new Set(nodes.map((n) => n.node_id));
  for (const [nid, agentsForNode] of byNode) {
    if (!knownIds.has(nid) && agentsForNode.length > 0) {
      ordered.push({ nodeId: nid, node: null, agents: agentsForNode });
    }
  }
  return ordered;
}

/**
 * Display name for a group, prioritising the operator-friendly hostname
 * over the opaque node_id. Falls back to the raw nodeId (e.g.
 * `"__unknown__"` for the orphan bucket) so the header always renders.
 */
export function nodeDisplayName(group: AgentNodeGroup): string {
  return group.node?.hostname ?? group.node?.node_id ?? group.nodeId;
}
