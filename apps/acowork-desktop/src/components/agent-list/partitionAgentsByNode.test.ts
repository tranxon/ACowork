/**
 * ADR-073 §4 unit tests for the remote-mode grouping helper.
 *
 * The sidebar's remote-mode rendering is correct *iff* the partition
 * function emits groups in the right order and never loses agents.
 * These tests pin the contract so future refactors don't silently
 * regress the visual grouping.
 */
import { describe, it, expect } from "vitest";
import { partitionAgentsByNode, nodeDisplayName, UNKNOWN_NODE_ID } from "./partitionAgentsByNode";
import type { AgentInfo, NodeInfo } from "../../lib/types";

// ── Fixtures ──────────────────────────────────────────────────────────
// Tiny constructor helpers keep each test focused on the *arrangement*
// of inputs and assertions, not on noise like `node_id` strings.

const agent = (over: Partial<AgentInfo>): AgentInfo => ({
  agent_id: "com.example.unknown",
  instance_id: "00000000-0000-0000-0000-000000000000",
  name: "agent",
  running: false,
  installed: true,
  ...over,
});

const node = (over: Partial<NodeInfo>): NodeInfo => ({
  node_id: "n1",
  online: true,
  capabilities: [],
  ...over,
});

describe("partitionAgentsByNode", () => {
  it("groups agents by node_id preserving input order within each bucket", () => {
    const a1 = agent({ instance_id: "a1", node_id: "n1" });
    const a2 = agent({ instance_id: "a2", node_id: "n2" });
    const a3 = agent({ instance_id: "a3", node_id: "n1" });
    const a4 = agent({ instance_id: "a4", node_id: "n2" });

    const groups = partitionAgentsByNode(
      [a1, a2, a3, a4],
      [node({ node_id: "n1" }), node({ node_id: "n2" })],
    );

    expect(groups).toHaveLength(2);
    expect(groups[0].nodeId).toBe("n1");
    expect(groups[0].agents.map((a) => a.instance_id)).toEqual(["a1", "a3"]);
    expect(groups[1].nodeId).toBe("n2");
    expect(groups[1].agents.map((a) => a.instance_id)).toEqual(["a2", "a4"]);
  });

  it("emits groups in the Gateway-provided `nodes` order, not in agent order", () => {
    // Agents appear in n1-then-n2 order, but the Gateway reports nodes
    // in n2-then-n1 order — the sidebar must follow the Gateway so the
    // user sees the same node ordering everywhere.
    const a1 = agent({ instance_id: "a1", node_id: "n1" });
    const a2 = agent({ instance_id: "a2", node_id: "n2" });

    const groups = partitionAgentsByNode(
      [a1, a2],
      [node({ node_id: "n2" }), node({ node_id: "n1" })],
    );

    expect(groups.map((g) => g.nodeId)).toEqual(["n2", "n1"]);
  });

  it("skips nodes that have no agents", () => {
    const a1 = agent({ instance_id: "a1", node_id: "n1" });
    const groups = partitionAgentsByNode(
      [a1],
      [node({ node_id: "n1" }), node({ node_id: "n2" })],
    );

    expect(groups).toHaveLength(1);
    expect(groups[0].nodeId).toBe("n1");
  });

  it("routes agents whose node_id is missing into a trailing 'unknown' bucket", () => {
    const a1 = agent({ instance_id: "a1", node_id: "n1" });
    const aOrphan = agent({ instance_id: "ao1", node_id: undefined });

    const groups = partitionAgentsByNode(
      [a1, aOrphan],
      [node({ node_id: "n1" })],
    );

    expect(groups).toHaveLength(2);
    expect(groups[0].nodeId).toBe("n1");
    expect(groups[0].agents.map((a) => a.instance_id)).toEqual(["a1"]);
    expect(groups[1].nodeId).toBe(UNKNOWN_NODE_ID);
    expect(groups[1].node).toBeNull();
    expect(groups[1].agents.map((a) => a.instance_id)).toEqual(["ao1"]);
  });

  it("routes agents whose node_id is not in the Gateway's node list into the unknown bucket", () => {
    // Could happen if a Node goes offline / un-enrolls while its agents
    // are still listed in the Registry. Surface them rather than hide.
    const aKnown = agent({ instance_id: "a1", node_id: "n1" });
    const aStale = agent({ instance_id: "a2", node_id: "n-stale" });

    const groups = partitionAgentsByNode(
      [aKnown, aStale],
      [node({ node_id: "n1" })],
    );

    expect(groups).toHaveLength(2);
    expect(groups[1].nodeId).toBe("n-stale");
    expect(groups[1].node).toBeNull();
    expect(groups[1].agents.map((a) => a.instance_id)).toEqual(["a2"]);
  });

  it("returns an empty array when there are no agents", () => {
    const groups = partitionAgentsByNode([], [node({ node_id: "n1" })]);
    expect(groups).toEqual([]);
  });

  it("keeps each agent's real node_id as the bucket key when the Gateway hasn't reported nodes yet", () => {
    // Transient state on app boot: the agent list snapshot has arrived
    // but the node list request is still in flight. We surface the
    // agents under their reported node_id (rather than collapsing them
    // into a synthetic "__unknown__" bucket) so the rendering is stable
    // when the node list arrives a moment later.
    const a1 = agent({ instance_id: "a1", node_id: "n1" });
    const a2 = agent({ instance_id: "a2", node_id: "n2" });
    const groups = partitionAgentsByNode([a1, a2], []);
    expect(groups).toHaveLength(2);
    expect(groups.map((g) => g.nodeId)).toEqual(["n1", "n2"]);
    expect(groups.every((g) => g.node === null)).toBe(true);
  });
});

describe("nodeDisplayName", () => {
  it("prefers hostname over node_id", () => {
    const g = {
      nodeId: "abc-123",
      node: node({ node_id: "abc-123", hostname: "macbook-pro.local" }),
      agents: [],
    };
    expect(nodeDisplayName(g)).toBe("macbook-pro.local");
  });

  it("falls back to node_id when hostname is absent", () => {
    const g = {
      nodeId: "abc-123",
      node: node({ node_id: "abc-123", hostname: undefined }),
      agents: [],
    };
    expect(nodeDisplayName(g)).toBe("abc-123");
  });

  it("falls back to the bucket key for the unknown bucket", () => {
    const g = {
      nodeId: UNKNOWN_NODE_ID,
      node: null,
      agents: [],
    };
    expect(nodeDisplayName(g)).toBe(UNKNOWN_NODE_ID);
  });
});
