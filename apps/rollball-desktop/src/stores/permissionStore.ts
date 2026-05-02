import { create } from "zustand";
import type { ToolApprovalNeededEvent } from "../lib/types";
import { getGatewayUrl } from "../lib/config";

interface PermissionStore {
  // Pending approval request queue
  pendingRequests: ToolApprovalNeededEvent[];
  // Current displayed approval request (modal)
  currentRequest: ToolApprovalNeededEvent | null;
  // Session-level allowed tools
  sessionAllowed: Set<string>;

  loading: boolean;

  // Actions
  showApprovalDialog: (event: ToolApprovalNeededEvent) => void;
  approve: (
    agentId: string,
    requestId: string,
    action: "allow" | "deny" | "allow_all_session",
  ) => Promise<void>;
  dismissCurrent: () => void;
  clearAll: () => void;
}

export const usePermissionStore = create<PermissionStore>((set, get) => ({
  pendingRequests: [],
  currentRequest: null,
  sessionAllowed: new Set(),
  loading: false,

  showApprovalDialog: (event) => {
    const { sessionAllowed } = get();
    // If tool is already session-approved, auto-approve
    if (sessionAllowed.has(event.tool_name)) {
      // Auto-approve without showing dialog
      get().approve(event.agent_id, event.request_id, "allow");
      return;
    }
    // Show dialog
    set((s) => {
      if (s.currentRequest === null) {
        return { currentRequest: event, pendingRequests: s.pendingRequests };
      }
      // Queue if another dialog is showing
      return { pendingRequests: [...s.pendingRequests, event] };
    });
  },

  approve: async (agentId, requestId, action) => {
    set({ loading: true });
    try {
      const res = await fetch(`${getGatewayUrl()}/api/agents/${agentId}/permissions/approve`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ request_id: requestId, action }),
      });
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      await res.json();

      if (action === "allow_all_session") {
        const current = get().currentRequest;
        if (current) {
          set((s) => {
            const newSet = new Set(s.sessionAllowed);
            newSet.add(current.tool_name);
            return { sessionAllowed: newSet };
          });
        }
      }
    } catch (e) {
      console.error("Failed to send approval:", e);
    } finally {
      // Show next pending request or clear
      set((s) => {
        const next = s.pendingRequests[0] || null;
        return {
          loading: false,
          currentRequest: next,
          pendingRequests: next ? s.pendingRequests.slice(1) : [],
        };
      });
    }
  },

  dismissCurrent: () => {
    set((s) => {
      const next = s.pendingRequests[0] || null;
      return {
        currentRequest: next,
        pendingRequests: next ? s.pendingRequests.slice(1) : [],
      };
    });
  },

  clearAll: () =>
    set({
      pendingRequests: [],
      currentRequest: null,
      sessionAllowed: new Set(),
    }),
}));
