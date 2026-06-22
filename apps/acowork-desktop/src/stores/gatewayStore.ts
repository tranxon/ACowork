import { create } from "zustand";
import { getGatewayUrl } from "../lib/config";
import type { HealthResponse, GatewayStatus, LocalGatewayState } from "../lib/types";

interface GatewayStore {
  status: GatewayStatus;
  health: HealthResponse | null;
  localState: LocalGatewayState;
  checkHealth: () => Promise<void>;
  startLocalGateway: () => Promise<void>;
  stopLocalGateway: () => Promise<void>;
  checkLocalStatus: () => Promise<void>;
}

export const useGatewayStore = create<GatewayStore>((set, get) => ({
  status: "disconnected",
  health: null,
  localState: "idle",

  checkHealth: async () => {
    try {
      const resp = await fetch(`${getGatewayUrl()}/health`);
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      const health = await resp.json() as HealthResponse;
      set({ status: "connected", health });
    } catch {
      set({ status: "error", health: null });
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
      console.error("Failed to start local gateway:", err);
      set({ localState: "error" });
    }
  },

  stopLocalGateway: async () => {
    try {
      const { invoke } = await import("@tauri-apps/api/core");
      await invoke("stop_local_gateway");
      set({ localState: "stopped", status: "disconnected", health: null });
    } catch (err) {
      console.error("Failed to stop local gateway:", err);
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
}));
