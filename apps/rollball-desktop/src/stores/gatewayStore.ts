import { create } from "zustand";
import { getGatewayUrl } from "../lib/config";
import type { HealthResponse, GatewayStatus } from "../lib/types";

interface GatewayStore {
  status: GatewayStatus;
  health: HealthResponse | null;
  checkHealth: () => Promise<void>;
}

export const useGatewayStore = create<GatewayStore>((set) => ({
  status: "disconnected",
  health: null,

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
}));
