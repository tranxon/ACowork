import { create } from "zustand";
import type { ToolApprovalNeededEvent } from "../lib/types";
import { getGatewayUrl } from "../lib/config";

interface PermissionStore {
  // Pending approval request queue
  pendingRequests: ToolApprovalNeededEvent[];
  // Current displayed approval request (modal)
  currentRequest: ToolApprovalNeededEvent | null;
  // Session-level allowed tools — scoped per session to prevent cross-session bleeding
  sessionAllowed: Record<string, Set<string>>;

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
  /** Check if a tool is session-allowed for a specific session */
  isSessionAllowed: (toolName: string, sessionId?: string | null) => boolean;
}

export const usePermissionStore = create<PermissionStore>((set, get) => ({
  pendingRequests: [],
  currentRequest: null,
  sessionAllowed: {},
  loading: false,
  approvalError: null,

  isSessionAllowed: (toolName: string, sessionId?: string | null) => {
    if (!sessionId) return false;
    return get().sessionAllowed[sessionId]?.has(toolName) ?? false;
  },

  showApprovalDialog: (event) => {
    // If tool is already session-approved for THIS session, auto-approve
    const sessionId = event.session_id;
    if (sessionId && get().sessionAllowed[sessionId]?.has(event.tool_name)) {
      void sendApprovalToGateway(event.agent_id, event.request_id, "allow", event.session_id);
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
      return { pendingRequests: [...s.pendingRequests, event] };
    });
  },

  approve: async (requestId, action) => {
    set({ loading: true, approvalError: null });

    const current = get().currentRequest;

    if (action === "allow_all_session" && current) {
      const sessionId = current.session_id;
      if (sessionId) {
        set((s) => {
          const existing = s.sessionAllowed[sessionId] ?? new Set<string>();
          const newSet = new Set(existing);
          newSet.add(current.tool_name);
          return {
            sessionAllowed: { ...s.sessionAllowed, [sessionId]: newSet },
          };
        });
      }
    }

    const agentId = current?.agent_id;
    if (agentId) {
      const sessionId = current?.session_id;
      const result = await sendApprovalToGateway(agentId, requestId, action, sessionId);
      if (!result.ok) {
        const errorMsg = result.status === 404
          ? "审批请求已过期（Runtime 已超时拒绝），操作未生效"
          : `审批发送失败 (HTTP ${result.status})`;
        set({ loading: false, approvalError: errorMsg });
        return;
      }
    }

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
      sessionAllowed: {},
      approvalError: null,
    }),
}));

async function sendApprovalToGateway(
  agentId: string,
  requestId: string,
  action: string,
  sessionId?: string | null,
): Promise<{ ok: boolean; status: number }> {
  try {
    const url = `${getGatewayUrl()}/api/agents/${encodeURIComponent(agentId)}/approval`;
    const resp = await fetch(url, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        request_id: requestId,
        action,
        ...(sessionId ? { session_id: sessionId } : {}),
      }),
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
