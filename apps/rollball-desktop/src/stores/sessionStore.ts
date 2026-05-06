import { create } from "zustand";
import type { SessionInfo } from "../lib/types";
import { getGatewayUrl } from "../lib/config";
import { useChatStore } from "./chatStore";

interface SessionState {
  sessions: SessionInfo[];
  currentSessionId: string | null;
  isLoading: boolean;
  isSessionPanelOpen: boolean;
  /** Latest session title per agent_id */
  sessionTitles: Record<string, string | null>;
  /** Remembers the last selected session per agent, survives component remount */
  agentSessionMap: Record<string, string>;

  // Actions
  fetchSessions: (agentId: string) => Promise<void>;
  fetchLatestSessionTitle: (agentId: string) => Promise<string | null>;
  switchSession: (sessionId: string, agentId?: string) => Promise<void>;
  saveSessionForAgent: (agentId: string, sessionId: string) => void;
  createSession: (agentId: string) => Promise<void>;
  deleteSession: (agentId: string, sessionId: string) => Promise<void>;
  setSessionPanelOpen: (open: boolean) => void;
  toggleSessionPanel: () => void;
  /** Update a session's title locally (no API call) */
  updateSessionTitle: (sessionId: string, title: string) => void;
  reset: () => void;
}

export const useSessionStore = create<SessionState>((set) => ({
  sessions: [],
  currentSessionId: null,
  isLoading: false,
  isSessionPanelOpen: false,
  sessionTitles: {},
  agentSessionMap: {},

  fetchSessions: async (agentId: string) => {
    set({ isLoading: true }); // Keep old sessions visible while loading
    try {
      const resp = await fetch(`${getGatewayUrl()}/api/agents/${agentId}/sessions`);
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      const data = (await resp.json()) as { sessions: SessionInfo[] };
      const sessions = (data.sessions ?? []).sort(
        (a, b) =>
          new Date(b.created_at).getTime() - new Date(a.created_at).getTime(),
      );
      // Update session title for this agent
      const title = sessions.length > 0 ? (sessions[0]?.title ?? "") : null;
      set((state) => ({
        sessions,
        isLoading: false,
        sessionTitles: { ...state.sessionTitles, [agentId]: title },
      }));
    } catch (e) {
      console.error("[SessionStore] Failed to fetch sessions:", e);
      set({ sessions: [], isLoading: false });
    }
  },

  fetchLatestSessionTitle: async (agentId: string) => {
    try {
      const resp = await fetch(`${getGatewayUrl()}/api/agents/${agentId}/sessions?page=1&size=1`);
      if (!resp.ok) return null;
      const data = (await resp.json()) as { sessions: SessionInfo[] };
      const session = data.sessions?.[0];
      if (!session) return null;
      const title = session ? (session.title ?? "") : null;
      set((state) => ({
        sessionTitles: { ...state.sessionTitles, [agentId]: title },
      }));
      return title;
    } catch {
      return null;
    }
  },

  switchSession: async (sessionId: string, agentId?: string) => {
    // Cancel any in-flight session message loading before switching
    useChatStore.getState().abortSessionLoad();

    // Notify Runtime to switch its active ConversationSession (S1.14)
    if (agentId) {
      try {
        const resp = await fetch(
          `${getGatewayUrl()}/api/agents/${agentId}/sessions/${sessionId}/activate`,
          { method: "POST" },
        );
        if (!resp.ok) {
          console.warn(`[SessionStore] activate_session failed: HTTP ${resp.status}`);
          // Continue with local state update anyway — best-effort
        }
      } catch (e) {
        console.warn("[SessionStore] activate_session failed:", e);
        // Continue with local state update — best-effort
      }
    }
    set({ currentSessionId: sessionId });
  },

  saveSessionForAgent: (agentId: string, sessionId: string) => {
    set((state) => ({
      agentSessionMap: { ...state.agentSessionMap, [agentId]: sessionId },
    }));
  },

  createSession: async (agentId: string) => {
    try {
      const resp = await fetch(
        `${getGatewayUrl()}/api/agents/${agentId}/sessions`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
        },
      );
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      const data = (await resp.json()) as { session_id: string };
      const newSession: SessionInfo = {
        session_id: data.session_id,
        created_at: new Date().toISOString(),
        message_count: 0,
        title: null,
        status: "active",
      };
      set((state) => ({
        sessions: [newSession, ...state.sessions],
        currentSessionId: data.session_id,
      }));
      // Clear chatStore messages immediately so old session's content doesn't leak
      useChatStore.getState().clearMessages();
      useChatStore.setState({
        hasMoreMessages: false,
        messageCursor: null,
        isLoadingMore: false,
      });
    } catch (e) {
      console.error("[SessionStore] Failed to create session:", e);
    }
  },

  deleteSession: async (agentId: string, sessionId: string) => {
    try {
      const resp = await fetch(
        `${getGatewayUrl()}/api/agents/${agentId}/sessions/${sessionId}`,
        { method: "DELETE" },
      );
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      const data = (await resp.json()) as { deleted: boolean; session_id: string; new_session_id?: string };

      // Remove the deleted session from local state
      const isCurrent = useSessionStore.getState().currentSessionId === sessionId;
      const remaining = useSessionStore.getState().sessions.filter((s) => s.session_id !== sessionId);
      const newCurrentId = isCurrent
        ? (data.new_session_id || (remaining.length > 0 ? remaining[0].session_id : null))
        : useSessionStore.getState().currentSessionId;

      set({
        sessions: remaining,
        currentSessionId: newCurrentId,
      });

      // If the deleted session was current, clear chat and update agent map
      if (isCurrent) {
        useChatStore.getState().clearMessages();
        useChatStore.setState({
          hasMoreMessages: false,
          messageCursor: null,
          isLoadingMore: false,
        });
        if (newCurrentId) {
          useSessionStore.getState().saveSessionForAgent(agentId, newCurrentId);
        }
      }

      // Invalidate session title so it gets re-fetched
      set((state) => ({
        sessionTitles: { ...state.sessionTitles, [agentId]: undefined as any },
      }));
    } catch (e) {
      console.error("[SessionStore] Failed to delete session:", e);
    }
  },

  setSessionPanelOpen: (open: boolean) => {
  },

  toggleSessionPanel: () => {
    set((state) => ({ isSessionPanelOpen: !state.isSessionPanelOpen }));
  },

  updateSessionTitle: (sessionId: string, title: string) => {
    set((state) => ({
      sessions: state.sessions.map((s) =>
        s.session_id === sessionId && (!s.title || s.title.trim() === "") ? { ...s, title } : s,
      ),
    }));
  },

  reset: () => {
    set((state) => ({
      sessions: [],
      currentSessionId: null,
      isLoading: false,
      sessionTitles: {},
      agentSessionMap: state.agentSessionMap,
      isSessionPanelOpen: false,
    }));
  },
}));
