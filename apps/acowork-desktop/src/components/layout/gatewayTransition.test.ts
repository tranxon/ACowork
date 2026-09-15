/**
 * Self-check for the Gateway-status transition handler.
 *
 * Background: the MQTT broker lives inside the Gateway, so when the
 * Gateway goes away we stop receiving `agent_status` events. Without
 * this transition handler, `agent.alive` would stay `true` and the chat
 * panel would keep showing the previous session's content. On reconnect,
 * the agent list needs to be re-pulled because Gateway restart
 * re-spawns the system agent and clears the registry snapshot.
 *
 * Properties verified:
 *   1. First render (prev=null) is a no-op regardless of next.
 *   2. `connected → non-connected` flips every known agent offline AND
 *      releases its session runtime state (so attachment blobs can be
 *      GC'd — see `clearAgentSessions` for the GC contract).
 *   3. `non-connected → connected` triggers `fetchAgents` to reconcile
 *      and re-hydrate the user's last session.
 *   4. A `connected → connecting → connected` flicker does NOT trip a
 *      false drop — the prev ref guards against it.
 *   5. `connecting → error` and other non-`connected` transitions are
 *      no-ops (already in the disconnected bucket).
 */
import { describe, it, expect, vi } from "vitest";
import {
  applyGatewayTransition,
  onMqttConnectionEdge,
  type GatewayTransitionActions,
} from "./gatewayTransition";

function makeActions(): GatewayTransitionActions & {
  setAgentOffline: ReturnType<typeof vi.fn>;
  clearAgentSessions: ReturnType<typeof vi.fn>;
  refreshServices: ReturnType<typeof vi.fn>;
  markNodesOffline: ReturnType<typeof vi.fn>;
  fetchAgents: ReturnType<typeof vi.fn>;
  fetchNodes: ReturnType<typeof vi.fn>;
  getAgentIds: ReturnType<typeof vi.fn>;
} {
  return {
    getAgentIds: vi.fn(() => []),
    setAgentOffline: vi.fn(),
    clearAgentSessions: vi.fn(),
    refreshServices: vi.fn(),
    markNodesOffline: vi.fn(),
    fetchAgents: vi.fn(async () => undefined),
    fetchNodes: vi.fn(async () => undefined),
  };
}

describe("applyGatewayTransition", () => {
  it("is a no-op on first render (prev=null) regardless of next", () => {
    const a = makeActions();
    applyGatewayTransition(null, "connected", a);
    applyGatewayTransition(null, "disconnected", a);
    applyGatewayTransition(null, "error", a);
    expect(a.setAgentOffline).not.toHaveBeenCalled();
    expect(a.clearAgentSessions).not.toHaveBeenCalled();
    expect(a.refreshServices).not.toHaveBeenCalled();
    expect(a.markNodesOffline).not.toHaveBeenCalled();
    expect(a.fetchAgents).not.toHaveBeenCalled();
    expect(a.fetchNodes).not.toHaveBeenCalled();
  });

  it("on connected→error marks every agent offline AND clears its sessions", () => {
    const a = makeActions();
    a.getAgentIds.mockReturnValue(["agent-a", "agent-b", "agent-c"]);
    applyGatewayTransition("connected", "error", a);
    expect(a.setAgentOffline).toHaveBeenCalledTimes(3);
    expect(a.setAgentOffline).toHaveBeenNthCalledWith(1, "agent-a");
    expect(a.setAgentOffline).toHaveBeenNthCalledWith(2, "agent-b");
    expect(a.setAgentOffline).toHaveBeenNthCalledWith(3, "agent-c");
    expect(a.clearAgentSessions).toHaveBeenCalledTimes(3);
    expect(a.clearAgentSessions).toHaveBeenNthCalledWith(1, "agent-a");
    expect(a.clearAgentSessions).toHaveBeenNthCalledWith(2, "agent-b");
    expect(a.clearAgentSessions).toHaveBeenNthCalledWith(3, "agent-c");
    // The services diagnostic report must also be re-probed on drop —
    // the Gateway hosted every probe target, so a fresh pass yields the
    // honest all-offline report instead of the stale snapshot. The node
    // topology snapshot goes offline too (no new `bootstrap-state` will
    // arrive to refresh it — the broker died with the Gateway).
    expect(a.refreshServices).toHaveBeenCalledTimes(1);
    expect(a.markNodesOffline).toHaveBeenCalledTimes(1);
    expect(a.fetchAgents).not.toHaveBeenCalled();
    expect(a.fetchNodes).not.toHaveBeenCalled();
  });

  it("on connected→disconnected (same drop bucket as error)", () => {
    const a = makeActions();
    a.getAgentIds.mockReturnValue(["agent-x"]);
    applyGatewayTransition("connected", "disconnected", a);
    expect(a.setAgentOffline).toHaveBeenCalledWith("agent-x");
    expect(a.clearAgentSessions).toHaveBeenCalledWith("agent-x");
    expect(a.refreshServices).toHaveBeenCalledTimes(1);
    expect(a.markNodesOffline).toHaveBeenCalledTimes(1);
  });

  it("on connected→connecting (transient reconnect-in-progress)", () => {
    const a = makeActions();
    a.getAgentIds.mockReturnValue(["agent-x"]);
    applyGatewayTransition("connected", "connecting", a);
    expect(a.setAgentOffline).toHaveBeenCalledWith("agent-x");
    expect(a.clearAgentSessions).toHaveBeenCalledWith("agent-x");
    expect(a.refreshServices).toHaveBeenCalledTimes(1);
    expect(a.markNodesOffline).toHaveBeenCalledTimes(1);
  });

  it("on non-connected→connected triggers fetchAgents AND a services re-probe AND a nodes refetch", () => {
    const a = makeActions();
    a.getAgentIds.mockReturnValue(["agent-x"]);
    applyGatewayTransition("error", "connected", a);
    expect(a.fetchAgents).toHaveBeenCalledTimes(1);
    expect(a.fetchNodes).toHaveBeenCalledTimes(1);
    expect(a.setAgentOffline).not.toHaveBeenCalled();
    expect(a.clearAgentSessions).not.toHaveBeenCalled();
    expect(a.markNodesOffline).not.toHaveBeenCalled();
    // The diagnostic report recovers the real (live) state on rise.
    expect(a.refreshServices).toHaveBeenCalledTimes(1);
  });

  it("a connected→connecting→connected flicker does NOT fire drop or rise twice", () => {
    // Simulates the AppLayout effect running twice in quick succession
    // (one tick for `connecting`, one for `connected` after a watchdog
    // recovery). The user-visible outcome: agents stay alive, no
    // sessions are cleared, and fetchAgents fires exactly once on the
    // rise.
    const a = makeActions();
    a.getAgentIds.mockReturnValue(["agent-x"]);

    // tick 1: connected → connecting
    applyGatewayTransition("connected", "connecting", a);
    // tick 2: connecting → connected (back to healthy)
    applyGatewayTransition("connecting", "connected", a);

    expect(a.setAgentOffline).toHaveBeenCalledTimes(1);
    expect(a.setAgentOffline).toHaveBeenCalledWith("agent-x");
    expect(a.clearAgentSessions).toHaveBeenCalledTimes(1);
    expect(a.clearAgentSessions).toHaveBeenCalledWith("agent-x");
    expect(a.refreshServices).toHaveBeenCalledTimes(2); // once on drop, once on rise
    expect(a.markNodesOffline).toHaveBeenCalledTimes(1); // drop only
    expect(a.fetchAgents).toHaveBeenCalledTimes(1); // rise only
    expect(a.fetchNodes).toHaveBeenCalledTimes(1); // rise only
  });

  it("a connected→disconnected→connected sequence (full restart) flips offline then re-fetches", () => {
    const a = makeActions();
    a.getAgentIds.mockReturnValue(["agent-x"]);

    applyGatewayTransition("connected", "disconnected", a);
    expect(a.setAgentOffline).toHaveBeenCalledTimes(1);
    expect(a.clearAgentSessions).toHaveBeenCalledTimes(1);
    expect(a.refreshServices).toHaveBeenCalledTimes(1);
    expect(a.markNodesOffline).toHaveBeenCalledTimes(1);

    a.setAgentOffline.mockClear();
    a.clearAgentSessions.mockClear();
    a.refreshServices.mockClear();
    a.markNodesOffline.mockClear();

    applyGatewayTransition("disconnected", "connected", a);
    expect(a.setAgentOffline).not.toHaveBeenCalled();
    expect(a.clearAgentSessions).not.toHaveBeenCalled();
    expect(a.markNodesOffline).not.toHaveBeenCalled();
    // Rise re-probes services, re-fetches agents AND resyncs the node
    // topology (the drop edge marked it offline).
    expect(a.refreshServices).toHaveBeenCalledTimes(1);
    expect(a.fetchAgents).toHaveBeenCalledTimes(1);
    expect(a.fetchNodes).toHaveBeenCalledTimes(1);
  });

  it("no-op for connecting→error or other within non-connected bucket transitions", () => {
    const a = makeActions();
    applyGatewayTransition("connecting", "error", a);
    applyGatewayTransition("error", "disconnected", a);
    applyGatewayTransition("disconnected", "connecting", a);
    expect(a.setAgentOffline).not.toHaveBeenCalled();
    expect(a.clearAgentSessions).not.toHaveBeenCalled();
    expect(a.refreshServices).not.toHaveBeenCalled();
    expect(a.markNodesOffline).not.toHaveBeenCalled();
    expect(a.fetchAgents).not.toHaveBeenCalled();
    expect(a.fetchNodes).not.toHaveBeenCalled();
  });

  it("drop with zero agents still re-probes services AND marks nodes offline (Gateway is gone regardless)", () => {
    const a = makeActions();
    a.getAgentIds.mockReturnValue([]);
    applyGatewayTransition("connected", "error", a);
    expect(a.setAgentOffline).not.toHaveBeenCalled();
    expect(a.clearAgentSessions).not.toHaveBeenCalled();
    // Even with no agents, the diagnostic report must not stay stale —
    // and the node topology snapshot must not keep showing dead nodes.
    expect(a.refreshServices).toHaveBeenCalledTimes(1);
    expect(a.markNodesOffline).toHaveBeenCalledTimes(1);
  });
});

describe("onMqttConnectionEdge", () => {
  it("does NOT probe on first render (prev=null) — SplashScreen already verified", () => {
    const probe = vi.fn();
    onMqttConnectionEdge(null, true, probe);
    onMqttConnectionEdge(null, false, probe);
    expect(probe).not.toHaveBeenCalled();
  });

  it("does NOT probe when MQTT state is unchanged", () => {
    const probe = vi.fn();
    onMqttConnectionEdge(true, true, probe);
    onMqttConnectionEdge(false, false, probe);
    expect(probe).not.toHaveBeenCalled();
  });

  it("probes on drop edge (connected → disconnected) to distinguish gateway death from auto-sleep", () => {
    const probe = vi.fn();
    onMqttConnectionEdge(true, false, probe);
    expect(probe).toHaveBeenCalledTimes(1);
  });

  it("probes on rise edge (disconnected → connected) to converge gatewayStatus after restart", () => {
    const probe = vi.fn();
    onMqttConnectionEdge(false, true, probe);
    expect(probe).toHaveBeenCalledTimes(1);
  });

  it("probes exactly once per edge across a full drop→rise cycle", () => {
    const probe = vi.fn();
    onMqttConnectionEdge(true, false, probe); // drop
    onMqttConnectionEdge(false, false, probe); // settled down
    onMqttConnectionEdge(false, true, probe); // rise
    expect(probe).toHaveBeenCalledTimes(2);
  });
});