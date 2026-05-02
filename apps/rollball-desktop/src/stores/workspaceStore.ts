import { create } from "zustand";
import { useSettingsStore } from "./settingsStore";
import { DEFAULT_GATEWAY_URL } from "../lib/config";

/** Single workspace directory entry — matches Gateway API response */
interface WorkspaceDir {
  id: string;
  path: string;
  alias: string | null;
  access: "read-only" | "read-write";
  added_at: string;
  is_current: boolean;
  select_count: number;
  last_selected_at: string | null;
}

interface WorkspaceState {
  workspaces: WorkspaceDir[];
  currentWorkspaceId: string | null;
  loading: boolean;

  // Fetch workspace list for a given agent
  fetchWorkspaces: (agentId: string) => Promise<void>;

  // Set the current workspace (PUT API + local state update)
  setCurrentWorkspace: (agentId: string, workspaceId: string) => Promise<void>;

  // Clear state on agent switch
  reset: () => void;
}

/** Helper: resolve Gateway URL from settings store, fallback to default */
function getGatewayUrl(): string {
  return useSettingsStore.getState().gatewayUrl || DEFAULT_GATEWAY_URL;
}

/** Monotonic counter to discard stale async responses (race-condition guard) */
let requestSeq = 0;

export const useWorkspaceStore = create<WorkspaceState>((set, get) => ({
  workspaces: [],
  currentWorkspaceId: null,
  loading: false,

  fetchWorkspaces: async (agentId: string) => {
    const seq = ++requestSeq;
    set({ loading: true });
    try {
      const baseUrl = getGatewayUrl();
      const resp = await fetch(`${baseUrl}/api/agents/${agentId}/workspaces`);
      if (!resp.ok) {
        console.error("[WorkspaceStore] fetchWorkspaces failed:", resp.status, resp.statusText);
        set({ loading: false });
        return;
      }
      const data = (await resp.json()) as { workspaces: WorkspaceDir[] };
      const workspaces = data.workspaces || [];
      const current = workspaces.find((w) => w.is_current);
      // Discard stale response if a newer request has been issued
      if (seq !== requestSeq) return;
      set({
        workspaces,
        currentWorkspaceId: current?.id ?? null,
        loading: false,
      });
    } catch (e) {
      console.error("[WorkspaceStore] fetchWorkspaces error:", e);
      if (seq !== requestSeq) return;
      set({ loading: false });
    }
  },

  setCurrentWorkspace: async (agentId: string, workspaceId: string) => {
    const seq = ++requestSeq;
    const prevWorkspaces = get().workspaces;
    const prevCurrentId = get().currentWorkspaceId;
    try {
      const baseUrl = getGatewayUrl();
      const resp = await fetch(`${baseUrl}/api/agents/${agentId}/workspaces/current`, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ workspace_id: workspaceId }),
      });
      if (!resp.ok) {
        console.error("[WorkspaceStore] setCurrentWorkspace failed:", resp.status, resp.statusText);
        return;
      }
      // API returns the updated workspace list after switching
      const data = (await resp.json()) as { workspaces: WorkspaceDir[] };
      const workspaces = data.workspaces || [];
      const current = workspaces.find((w) => w.is_current);
      // Discard stale response if a newer request has been issued
      if (seq !== requestSeq) return;
      set({
        workspaces,
        currentWorkspaceId: current?.id ?? workspaceId,
      });
    } catch (e) {
      console.error("[WorkspaceStore] setCurrentWorkspace error:", e);
      // Revert to previous state on failure (only if still the latest request)
      if (seq !== requestSeq) return;
      set({ workspaces: prevWorkspaces, currentWorkspaceId: prevCurrentId });
    }
  },

  reset: () => {
    set({ workspaces: [], currentWorkspaceId: null, loading: false });
  },
}));

export type { WorkspaceDir, WorkspaceState };
