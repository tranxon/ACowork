import { create } from "zustand";
import type { SessionInfo } from "../lib/types";
import { getGatewayUrl } from "../lib/config";

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
  switchSession: (sessionId: string) => void;
  saveSessionForAgent: (agentId: string, sessionId: string) => void;
  createSession: (agentId: string) => Promise<void>;
  setSessionPanelOpen: (open: boolean) => void;
  toggleSessionPanel: () => void;
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
    set({ isLoading: true, sessions: [] }); // Clear immediately to avoid stale data
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

  switchSession: (sessionId: string) => {
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
    } catch (e) {
      console.error("[SessionStore] Failed to create session:", e);
    }
  },

  setSessionPanelOpen: (open: boolean) => {
    set({ isSessionPanelOpen: open });
  },

  toggleSessionPanel: () => {
    set((state) => ({ isSessionPanelOpen: !state.isSessionPanelOpen }));
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
