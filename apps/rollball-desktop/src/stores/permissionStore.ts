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
  // Last approval error (set when Gateway returns non-2xx)
  approvalError: string | null;

  // Actions
  showApprovalDialog: (event: ToolApprovalNeededEvent) => void;
  approve: (
    requestId: string,
    action: "allow" | "deny" | "allow_all_session",
  ) => Promise<void>;
  dismissCurrent: () => void;
  clearApprovalError: () => void;
  clearAll: () => void;
}

export const usePermissionStore = create<PermissionStore>((set, get) => ({
  pendingRequests: [],
  currentRequest: null,
  sessionAllowed: new Set(),
  loading: false,
  approvalError: null,

  showApprovalDialog: (event) => {
    const { sessionAllowed } = get();
    // If tool is already session-approved, auto-approve without showing dialog
    if (sessionAllowed.has(event.tool_name)) {
      // Send approval to Gateway API directly, then advance queue
      void sendApprovalToGateway(event.agent_id, event.request_id, "allow");
      set((s) => {
        const next = s.pendingRequests[0] || null;
        return {
          loading: false,
          currentRequest: next,
          pendingRequests: next ? s.pendingRequests.slice(1) : [],
        };
      });
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

  approve: async (requestId, action) => {
    set({ loading: true, approvalError: null });

    const current = get().currentRequest;

    if (action === "allow_all_session") {
      if (current) {
        set((s) => {
          const newSet = new Set(s.sessionAllowed);
          newSet.add(current.tool_name);
          return { sessionAllowed: newSet };
        });
      }
    }

    // Send approval decision to Gateway API and await response
    const agentId = current?.agent_id;
    if (agentId) {
      const result = await sendApprovalToGateway(agentId, requestId, action);
      if (!result.ok) {
        // Gateway returned error (e.g. 404 = approval already timed out)
        const errorMsg = result.status === 404
          ? "审批请求已过期（Runtime 已超时拒绝），操作未生效"
          : `审批发送失败 (HTTP ${result.status})`;
        set({ loading: false, approvalError: errorMsg });
        return; // Keep modal visible so user sees the error
      }
    }

    // Success — advance to next pending request or clear
    set((s) => {
      const next = s.pendingRequests[0] || null;
      return {
        loading: false,
        approvalError: null,
        currentRequest: next,
        pendingRequests: next ? s.pendingRequests.slice(1) : [],
      };
    });
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

  clearApprovalError: () => set({ approvalError: null }),

  clearAll: () =>
    set({
      pendingRequests: [],
      currentRequest: null,
      sessionAllowed: new Set(),
      approvalError: null,
    }),
}));

/// Send tool approval decision to Gateway HTTP API.
/// This resolves the oneshot channel on the Gateway side,
/// which unblocks the gRPC dispatch handler waiting for the Runtime.
async function sendApprovalToGateway(
  agentId: string,
  requestId: string,
  action: string,
): Promise<{ ok: boolean; status: number }> {
  try {
    const url = `${getGatewayUrl()}/api/agents/${encodeURIComponent(agentId)}/approval`;
    const resp = await fetch(url, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ request_id: requestId, action }),
    });
    if (!resp.ok) {
      console.warn(
        `[PermissionStore] Approval API returned ${resp.status} for ${requestId}`,
      );
    }
    return { ok: resp.ok, status: resp.status };
  } catch (err) {
    console.error("[PermissionStore] Failed to send approval:", err);
    return { ok: false, status: 0 };
  }
}
