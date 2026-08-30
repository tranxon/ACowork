import { create } from "zustand";
import { getGatewayUrl } from "../lib/config";
import type { HealthResponse, GatewayStatus, LocalGatewayState, AgentMigrationProgress } from "../lib/types";
import { fetchMigrationProgress } from "../lib/gateway-api";
import { log } from "../lib/logger";

interface GatewayStore {
  status: GatewayStatus;
  health: HealthResponse | null;
  localState: LocalGatewayState;
  /** Migration progress for all agents (polled from Gateway) */
  migrationProgress: Record<string, AgentMigrationProgress>;
  checkHealth: () => Promise<void>;
  startLocalGateway: () => Promise<void>;
  stopLocalGateway: () => Promise<void>;
  checkLocalStatus: () => Promise<void>;
  /** Poll migration progress from Gateway, returns true if any migration is in progress */
  pollMigrationProgress: () => Promise<boolean>;
  /** Update migration progress for a single agent (from WebSocket event) */
  updateMigrationProgress: (agentId: string, reconstructed: number, totalScanned: number) => void;
}

export const useGatewayStore = create<GatewayStore>((set, get) => ({
  // ADR-051 + ADR-052 (lifecycle ownership):
  //   `SplashScreen` is the SOLE owner of startup-time health probing.
  //   It calls `checkHealth()` in a poll loop until the Gateway responds,
  //   then calls `onReady()` which mounts `AppLayout`. By the time
  //   `AppLayout` reads `status`, SplashScreen has already pushed it
  //   to `connected`. We start at `disconnected` so that any banner /
  //   indicator keyed on `status === "disconnected"` shows the right
  //   thing before SplashScreen takes over — but AppLayout is gated
  //   by `gatewayReady` in App.tsx, so no banner is visible during the
  //   startup window regardless of this initial value.
  status: "disconnected",
  health: null,
  localState: "idle",
  migrationProgress: {},

  checkHealth: async () => {
    const t0 = performance.now();
    const prev = get().status;
    try {
      const resp = await fetch(`${getGatewayUrl()}/health`);
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      const health = await resp.json() as HealthResponse;
      set({ status: "connected", health });
      log.debug(
        `[checkHealth] OK prev=${prev} → connected (${(performance.now() - t0).toFixed(1)}ms)`,
      );
    } catch (err) {
      const dur = (performance.now() - t0).toFixed(1);
      log.error(
        `[checkHealth] FAIL prev=${prev} → `,
        err instanceof Error ? `${err.message} (${dur}ms)` : `${err} (${dur}ms)`,
      );
      // ADR-051: distinguish startup probe failure (transient, gateway
      // is still booting) from steady-state drop (gateway was reachable
      // and just went away).
      //
      //   - Never connected yet  → keep `connecting` (or downgrade from
      //                            `error` if we transiently hit it).
      //   - Currently `connected`→ upgrade to `error` (genuine outage).
      //
      // We never set `error` for a fresh probe failure because the only
      // visible effect is a red status bar that disappears seconds later
      // once the gateway comes up — pure UX noise.
      if (prev === "connected") {
        set({ status: "error", health: null });
      } else {
        set({ status: "connecting", health: null });
      }
    }
  },

  startLocalGateway: async () => {
    // Sync with the Rust-side process handle before checking the guard.
    // The SplashScreen boot path calls `init_local_gateway` directly (not
    // this action), so `localState` may still be "idle" even though the
    // backend already has a running child process.
    await get().checkLocalStatus();
    if (get().localState === "starting") return;
    if (get().localState === "running") {
      // Gateway process already exists (e.g. from a previous session or
      // SplashScreen boot path), but we may not have checked health yet.
      // Without this call, `status` stays "disconnected" and the UI shows
      // "Not started" even though the Gateway is actually reachable.
      await get().checkHealth();
      return;
    }
    set({ localState: "starting" });
    try {
      // Dynamically import invoke to avoid issues when not in Tauri context
      const { invoke } = await import("@tauri-apps/api/core");
      await invoke("start_local_gateway");
      set({ localState: "running" });
      // Check health now that the local gateway is up
      await get().checkHealth();
    } catch (err) {
      log.error("Failed to start local gateway:", err);
      set({ localState: "error" });
    }
  },

  stopLocalGateway: async () => {
    try {
      const { invoke } = await import("@tauri-apps/api/core");
      await invoke("stop_local_gateway");
      set({ localState: "stopped", status: "disconnected", health: null });
    } catch (err) {
      log.error("Failed to stop local gateway:", err);
      set({ localState: "error" });
    }
  },

  checkLocalStatus: async () => {
    try {
      const { invoke } = await import("@tauri-apps/api/core");
      const running = await invoke<boolean>("get_local_gateway_status");
      set({ localState: running ? "running" : "stopped" });
    } catch {
      // Not in Tauri context (e.g. plain web dev mode) or command failed.
      // Leave localState unchanged so we don't clobber a valid "running"
      // state from a previous successful start.
    }
  },

  pollMigrationProgress: async () => {
    if (get().status !== "connected") return false;
    try {
      const resp = await fetchMigrationProgress();
      const progress: Record<string, AgentMigrationProgress> = {};
      let anyInProgress = false;
      for (const agent of resp.agents) {
        progress[agent.agent_id] = agent;
        if (!agent.done && !agent.error) anyInProgress = true;
      }
      set({ migrationProgress: progress });
      return anyInProgress;
    } catch {
      return false;
    }
  },

  updateMigrationProgress: (agentId: string, reconstructed: number, totalScanned: number) => {
    set((state) => {
      const existing = state.migrationProgress[agentId];
      if (!existing) return state;
      return {
        migrationProgress: {
          ...state.migrationProgress,
          [agentId]: {
            ...existing,
            progress: {
              rebuilt: reconstructed,
              total_scanned: totalScanned,
              errors: existing.progress?.errors ?? 0,
              phase: "reembed",
              label: existing.progress?.label ?? "",
            },
          },
        },
      };
    });
  },
}));
