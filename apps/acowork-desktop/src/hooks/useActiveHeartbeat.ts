/**
 * useActiveHeartbeat — periodic presence pulse for the idle-watcher.
 *
 * The Runtime's IdleWatcher (core/acowork-runtime/src/agent/idle_watcher.rs)
 * has two independent activity signals:
 *
 *   - `record_inbound()`  — event-driven, fires on each user action
 *                           (send/stop/switch/etc.) carried over MQTT
 *                           control commands.
 *   - `record_heartbeat()`— time-driven,  fires every ~15s while the
 *                           frontend is viewing this agent. Backs the
 *                           idle deadline renewal during "user is just
 *                           scrolling through message history" — the
 *                           inbound stream goes silent in that case.
 *
 * This hook is the frontend half of the heartbeat contract. It does NOT
 * know about the Runtime's heartbeat_timeout window, the idle deadline,
 * or any decision logic. It only broadcasts the objective fact "I am
 * alive and this agent is selected" at a fixed cadence. Crash safety is
 * automatic: if this hook's component unmounts, the React cleanup
 * stops the interval and heartbeats simply stop arriving — the
 * Runtime's freshness window will then expire and the watcher falls
 * back to inbound-based deadline accounting.
 *
 * Why 15 seconds:
 *   - Default Runtime idle timeout is 30 minutes (1800 s).
 *   - Runtime's heartbeat freshness window is `max(60s, idle/4) = 450s`
 *     (7.5 min), so up to ~30 missed heartbeats can be tolerated.
 *   - 15s cadence gives plenty of headroom against network jitter
 *     and React StrictMode double-mount while keeping MQTT overhead
 *     negligible (~2 messages/min/agent).
 *
 * Why emit IMMEDIATELY on mount (before the first setInterval fires):
 *   - When the user switches to a new agent, the previous hook's
 *     interval has been cleaned up and the new hook just mounted.
 *     Without an immediate pulse, the new agent would see up to 15s
 *     of "no heartbeat" before the first tick — long enough that a
 *     short idle timeout (e.g. 5 min) might mis-classify it.
 *   - The Runtime's heartbeat_timeout floor (60s) absorbs the
 *     remaining window comfortably.
 *
 * Selection binding:
 *   - The hook takes the agent ID as an argument. When the ID changes
 *     (user selects a different agent in the sidebar), React unmounts
 *     and remounts the effect → interval restarts cleanly for the new
 *     agent. This is intentional: the previous agent MUST stop
 *     sending heartbeats so the Runtime can recover its idle deadline.
 *   - A null ID (no agent selected) is a no-op — no interval, no
 *     invoke, no leaked timer.
 */

import { useEffect } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useAgentStore } from "../stores/agentStore";
import { log } from "../lib/logger";

/** Frontend heartbeat cadence. See hook docblock for rationale. */
export const ACTIVE_HEARTBEAT_INTERVAL_MS = 15_000;

/** MQTT control command name. Mirrors the Rust ControlAction variant. */
const COMMAND_ACTIVE_HEARTBEAT = "active_heartbeat";

/**
 * Start broadcasting `active_heartbeat` MQTT control commands for the
 * given agent ID at `ACTIVE_HEARTBEAT_INTERVAL_MS` cadence. Stops
 * automatically on unmount or when `agentId` changes / becomes null.
 *
 * Pure observer hook — no return value, no side effects on store state.
 * Errors from the Tauri invoke are logged at `debug` level (not `error`)
 * because a transient publish failure is non-fatal: the next interval
 * tick will retry, and the Runtime's freshness window absorbs short
 * gaps without false-positive auto-sleep.
 */
export function useActiveHeartbeat(agentId: string | null): void {
  useEffect(() => {
    if (!agentId) {
      return;
    }

    const sendHeartbeat = () => {
      void invoke("mqtt_publish_control", {
        agentId,
        command: COMMAND_ACTIVE_HEARTBEAT,
        payloadJson: {},
      }).catch((err: unknown) => {
        log.debug("[useActiveHeartbeat] publish failed (will retry next tick):", err);
      });
    };

    // Immediate pulse on mount so a newly selected agent doesn't wait
    // up to one full interval before the Runtime hears about it.
    sendHeartbeat();

    const intervalId = window.setInterval(sendHeartbeat, ACTIVE_HEARTBEAT_INTERVAL_MS);

    return () => {
      window.clearInterval(intervalId);
    };
  }, [agentId]);
}

/**
 * Convenience binding: drives `useActiveHeartbeat` from the agent
 * store's `selectedAgentId`. The intended single integration point —
 * call once at the app shell level (e.g. AppLayout) and the heartbeat
 * follows agent selection automatically.
 */
export function useActiveHeartbeatForSelection(): void {
  const selectedAgentId = useAgentStore((s) => s.selectedAgentId);
  useActiveHeartbeat(selectedAgentId);
}
