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

/** Tracks the latest fetch request to discard stale responses on agent switch */
let fetchSessionId = 0;

export const useSessionStore = create<SessionState>((set) => ({
  sessions: [],
  currentSessionId: null,
  isLoading: false,
  isSessionPanelOpen: false,
  sessionTitles: {},
  agentSessionMap: {},

  fetchSessions: async (agentId: string) => {
    // Cancel any in-flight fetch by bumping the id
    const requestId = ++fetchSessionId;
    set({ isLoading: true });
    try {
      const resp = await fetch(`${getGatewayUrl()}/api/agents/${agentId}/sessions`);
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      const data = (await resp.json()) as { sessions: SessionInfo[] };
      const sessions = (data.sessions ?? []).sort(
        (a, b) =>
          new Date(b.created_at).getTime() - new Date(a.created_at).getTime(),
      );
      // Discard stale response — a newer fetch has already started
      if (requestId !== fetchSessionId) return;
      const title = sessions.length > 0 ? (sessions[0]?.title ?? "") : null;
      set((state) => ({
        sessions,
        isLoading: false,
        sessionTitles: { ...state.sessionTitles, [agentId]: title },
      }));
    } catch (e) {
      // Discard stale error too
      if (requestId !== fetchSessionId) return;
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
    // No-op if switching to the already-active session
    if (sessionId === useSessionStore.getState().currentSessionId) return;

    // ① IMMEDIATELY set currentSessionId — closes the WS event vulnerability window.
    //    WS events will now be filtered/routed to the new session.
    set({ currentSessionId: sessionId });

    // ② Notify chatStore to activate this session (manages session-level state isolation)
    if (agentId) {
      useChatStore.getState().activateSession(agentId, sessionId);
    }

    // ③ Cancel any in-flight session message loading
    useChatStore.getState().abortSessionLoad();

    // ④ Notify Runtime to switch its active ConversationSession (best-effort, non-blocking)
    if (agentId) {
      fetch(
        `${getGatewayUrl()}/api/agents/${agentId}/sessions/${sessionId}/activate`,
        { method: "POST" },
      ).catch((e) => {
        console.warn("[SessionStore] activate_session failed:", e);
      });
    }
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
      // Activate the new session in chatStore (creates session state entry)
      useChatStore.getState().activateSession(agentId, data.session_id);
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

      // If the deleted session was current, activate the new current session
      if (isCurrent) {
        if (newCurrentId) {
          useChatStore.getState().activateSession(agentId, newCurrentId);
          useSessionStore.getState().saveSessionForAgent(agentId, newCurrentId);
        } else {
          useChatStore.getState().clearMessages(agentId);
        }
      }
      // Remove deleted session's cached state from chatStore
      useChatStore.getState().removeSessionState(agentId, sessionId);

      // Invalidate session title so it gets re-fetched
      set((state) => ({
        sessionTitles: { ...state.sessionTitles, [agentId]: undefined as any },
      }));
    } catch (e) {
      console.error("[SessionStore] Failed to delete session:", e);
    }
  },

  setSessionPanelOpen: (open: boolean) => {
    set({ isSessionPanelOpen: open });
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
    // Bump fetch id to cancel any in-flight fetch for the previous agent
    ++fetchSessionId;
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