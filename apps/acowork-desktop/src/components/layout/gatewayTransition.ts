import type { GatewayStatus } from "../../lib/types";

export interface GatewayTransitionActions {
  /** Snapshot of known agents (instance id → any). The helper only needs keys. */
  getAgentIds: () => string[];
  /** Patch an agent's liveness (mirrors `updateAgentLiveness(id, alive, false)`). */
  setAgentOffline: (agentId: string) => void;
  /** Drop every cached session's runtime state for an agent — releases
   *  the messages / pending approvals / tool progress / abort controllers
   *  so the underlying attachment blobs can be GC'd. */
  clearAgentSessions: (agentId: string) => void;
  /** Mark every node in the topology snapshot offline. Gateway drop
   *  edge: the snapshot is stale the moment the Gateway dies and no new
   *  `bootstrap-state` snapshot will arrive (the MQTT broker lived in
   *  the Gateway) — the sidebar node group headers would otherwise keep
   *  showing the dead nodes as online. */
  markNodesOffline: () => void;
  /** Re-run the services diagnostic probe. Called on BOTH edges: a
   *  Gateway drop makes every probe target unreachable, so a fresh
   *  pass yields the honest all-offline report (gateway_reachable
   *  false → the panel's red banner + red dots) instead of the stale
   *  pre-disconnect snapshot; a rise re-probes so the report recovers
   *  the real state. `diagnose()` never throws and self-throttles
   *  while a pass is in flight. */
  refreshServices: () => void;
  /** Reconcile the agent list + liveness from the Gateway. Called on
   *  the reconnect edge to pick up the freshly-respawned system agent. */
  fetchAgents: () => Promise<unknown>;
  /** Refetch the node topology from the Gateway. Called on the rise
   *  edge — the drop edge marked the snapshot offline, now resync it
   *  with the live topology. */
  fetchNodes: () => Promise<unknown>;
}

/**
 * Decide what to do on a Gateway-status transition.
 *
 *   prev=null                 → first render (SplashScreen already drove
 *                                status to `connected` before AppLayout
 *                                mounts); nothing to do.
 *   prev=connected, cur≠x    → "drop" edge: every known agent is marked
 *                                offline and its sessions are cleared,
 *                                mirroring the natural `agent_status
 *                                offline` MQTT flow that node-stop / an
 *                                individual stopAgent would have produced.
 *                                ChatPanel's `!selectedAgent.alive` gate
 *                                then renders the sleeping screen. The
 *                                services report is re-diagnosed so the
 *                                diagnostic panel shows the honest
 *                                all-offline state (e.g. node offline)
 *                                rather than the stale snapshot, and the
 *                                node topology snapshot is marked
 *                                offline (no new `bootstrap-state` will
 *                                arrive — the broker died with the
 *                                Gateway — so the sidebar group headers
 *                                would otherwise stay green).
 *   prev≠connected, cur=conn  → "rise" edge: pull the fresh agent list
 *                                from the Gateway (system agent gets
 *                                respawned on restart), re-probe the
 *                                services report, resync the node
 *                                topology, and let the normal
 *                                `selectAgent → fetchLatestSession →
 *                                openSession` chain re-hydrate the user's
 *                                last session.
 *   other                     → no-op (e.g. connecting→connecting, or the
 *                                transient `connected→connecting→
 *                                connected` flicker during a quick restart).
 *
 * Side-effect helper so the AppLayout effect body stays one-liner and
 * the edge-detection logic is unit-testable without rendering React.
 */
export function applyGatewayTransition(
  prev: GatewayStatus | null,
  next: GatewayStatus,
  actions: GatewayTransitionActions,
): void {
  if (prev === null) return;
  if (prev === "connected" && next !== "connected") {
    actions.refreshServices();
    actions.markNodesOffline();
    for (const id of actions.getAgentIds()) {
      actions.setAgentOffline(id);
      actions.clearAgentSessions(id);
    }
  } else if (prev !== "connected" && next === "connected") {
    actions.refreshServices();
    void actions.fetchAgents();
    void actions.fetchNodes();
  }
}

/**
 * Decide whether an MQTT connection-edge transition should trigger an
 * HTTP Gateway probe.
 *
 * Why this exists: `gatewayStore.status` is only refreshed by a
 * `checkHealth()` call, and nothing polls it during steady state. When
 * the Gateway is killed externally (CLI stop, crash), the FIRST signal
 * the frontend receives is the MQTT disconnect (broker lived in the
 * Gateway) — `gatewayStatus` would otherwise stay `connected` forever
 * and the `applyGatewayTransition` drop edge would never fire.
 *
 * So on every MQTT `connected ↔ not-connected` edge we re-probe HTTP:
 *   - drop edge: distinguishes "Gateway died" (probe fails →
 *     `gatewayStatus` flips to `error` → applyGatewayTransition fires
 *     the drop) from "agents just auto-slept" (probe succeeds →
 *     `gatewayStatus` stays `connected` → correctly no-op).
 *   - rise edge: after a Gateway restart the MQTT client reconnects
 *     before the user clicks anything; the probe flips `gatewayStatus`
 *     back to `connected` so the rise edge (fetchAgents) can fire.
 *
 * `prevUp === null` (first render) never probes — the SplashScreen boot
 * path already verified the Gateway.
 */
export function onMqttConnectionEdge(
  prevUp: boolean | null,
  up: boolean,
  probe: () => void,
): void {
  if (prevUp === null || prevUp === up) return;
  probe();
}