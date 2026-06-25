import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import { BUILTIN_ICON_IDS } from "../components/common/UserAvatar";
import { clearAgentAvatarCache } from "../lib/avatar";
import type { AgentInfo, AgentDetail, SessionInfo, SessionStatus } from "../lib/types";
import { isSessionActive } from "../lib/types";
import { getGatewayUrl } from "../lib/config";
import { useChatStore } from "./chatStore";
import { useWorkspaceStore } from "./workspaceStore";

/** System Agent ID — always auto-started by Gateway */
const SYSTEM_AGENT_ID = "com.acowork.system";

// ══════════════════════════════════════════════════════════════════════════
// AgentProfile types (moved from agentProfileStore.ts)
// ══════════════════════════════════════════════════════════════════════════

export interface AgentProfileSettings {
  displayName?: string;
  /** @deprecated ADR-017 — avatar is now server-side (agent_config.json).
   *  Kept for backward compat with existing localStorage profiles. */
  avatarIconId?: string | null;
  modelId?: string;
  providerId?: string;
  maxTokens?: number;
  maxIterations?: number;
  systemPrompt?: string;
  shellApprovalThreshold?: string;
  approvalTimeoutSecs?: number;
  globalMaxTokens?: number;
  activeModel?: string;
  activeProvider?: string;
}

const DEFAULT_PROFILE: AgentProfileSettings = {
  displayName: undefined,
  avatarIconId: null,
  modelId: undefined,
  providerId: undefined,
  maxTokens: 0,
  maxIterations: 0,
  systemPrompt: undefined,
  shellApprovalThreshold: undefined,
  approvalTimeoutSecs: undefined,
};

const STORAGE_KEY = "acowork-agent-profiles";

function loadAllProfiles(): Record<string, AgentProfileSettings> {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (raw) {
      const parsed = JSON.parse(raw) as Record<string, Partial<AgentProfileSettings>>;
      const result: Record<string, AgentProfileSettings> = {};
      for (const [agentId, s] of Object.entries(parsed)) {
        result[agentId] = normalizeProfile(s);
      }
      return result;
    }
  } catch {
    // localStorage unavailable or corrupted
  }
  return {};
}

function saveAllProfiles(profiles: Record<string, AgentProfileSettings>) {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(profiles));
  } catch {
    // silently ignore
  }
}

function normalizeProfile(s: Partial<AgentProfileSettings>): AgentProfileSettings {
  return {
    displayName: s.displayName,
    avatarIconId: validateIconId(s.avatarIconId),
    modelId: s.modelId,
    providerId: s.providerId,
    maxTokens: typeof s.maxTokens === "number" && s.maxTokens > 0 ? s.maxTokens : 0,
    maxIterations:
      typeof s.maxIterations === "number" && s.maxIterations > 0
        ? s.maxIterations
        : typeof (s as { toolsLimit?: number }).toolsLimit === "number" &&
          (s as { toolsLimit?: number }).toolsLimit! > 0
          ? (s as { toolsLimit?: number }).toolsLimit!
          : 0,
    systemPrompt: s.systemPrompt,
    shellApprovalThreshold: s.shellApprovalThreshold,
    approvalTimeoutSecs:
      typeof s.approvalTimeoutSecs === "number" && s.approvalTimeoutSecs > 0
        ? s.approvalTimeoutSecs
        : undefined,
    globalMaxTokens: typeof s.globalMaxTokens === "number" ? s.globalMaxTokens : undefined,
    activeModel: typeof s.activeModel === "string" ? s.activeModel : undefined,
    activeProvider: typeof s.activeProvider === "string" ? s.activeProvider : undefined,
  };
}

function validateIconId(id?: unknown): string | null | undefined {
  if (id === null || id === undefined) return id;
  if (typeof id === "string" && BUILTIN_ICON_IDS.includes(id)) return id;
  return null;
}

// ══════════════════════════════════════════════════════════════════════════
// AgentStorage — per-agent data container
// ══════════════════════════════════════════════════════════════════════════

export interface AgentStorage {
  /** Per-agent data: 一个 agent 的全部运行时状态 */
  meta: AgentInfo;
  /** User-customizable profile (persisted to localStorage) */
  profile: AgentProfileSettings;
  /** Sessions belonging to this agent */
  sessions: SessionInfo[];
  /** Latest session title (top of list) for the AgentList sidebar.
   *  undefined = not yet fetched (shows skeleton); null = fetched, no sessions;
   *  string = fetched, latest session title (empty string = untitled). */
  sessionTitle: string | null | undefined;
  /** Pagination for sessions list */
  pagination: {
    currentPage: number;
    totalPages: number;
    totalCount: number;
    pageSize: number;
  };
  /** Currently loading sessions for this agent */
  isLoading: boolean;
  /** Remembers the last active session per agent (survives remount) */
  rememberedSessionId: string | null;
}

const DEFAULT_PAGINATION = { currentPage: 1, totalPages: 1, totalCount: 0, pageSize: 20 };

function createStorage(meta: AgentInfo, profile: AgentProfileSettings): AgentStorage {
  return {
    meta,
    profile,
    sessions: [],
    sessionTitle: undefined,
    pagination: { ...DEFAULT_PAGINATION },
    isLoading: false,
    rememberedSessionId: null,
  };
}

/** Helper: patch a specific agent's storage fields inside agents map */
function patchAgent<S extends Partial<AgentStorage>>(
  state: { agents: Record<string, AgentStorage> },
  agentId: string,
  patch: S,
): { agents: Record<string, AgentStorage> } {
  const existing = state.agents[agentId];
  if (!existing) return { agents: state.agents };
  return {
    agents: {
      ...state.agents,
      [agentId]: { ...existing, ...patch },
    },
  };
}

// ══════════════════════════════════════════════════════════════════════════
// Module-level: in-flight request dedup
// ══════════════════════════════════════════════════════════════════════════

let fetchSessionReqId = 0;

// ══════════════════════════════════════════════════════════════════════════
// Store interface
// ══════════════════════════════════════════════════════════════════════════

interface AgentStoreState {
  // ── Data ──

  /** Unified per-agent storage: agentId → AgentStorage.
   *  Switching agents does NOT mutate this map — UI reads by `selectedAgentId`. */
  agents: Record<string, AgentStorage>;
  /** Currently selected agent ID — the "pointer" that UI uses to read agents[selectedAgentId]. */
  selectedAgentId: string | null;
  /** True once the user has explicitly chosen an agent (suppresses auto-select). */
  _userHasSelected: boolean;
  /** Loading flag for the master agent list */
  loading: boolean;
  /** Master list fetch error */
  error: string | null;
  /** Global UI state: whether the SessionPanel dropdown is open. (display-only, cleared on agent switch) */
  isSessionPanelOpen: boolean;

  // ── Agent meta actions ──

  fetchAgents: () => Promise<void>;
  selectAgent: (id: string | null) => void;
  installAgent: (packagePath: string) => Promise<void>;
  uninstallAgent: (agentId: string) => Promise<void>;
  startAgent: (agentId: string, devMode?: boolean) => Promise<void>;
  stopAgent: (agentId: string) => Promise<void>;
  restartAgentInDebug: (agentId: string) => Promise<void>;
  getAgentDetail: (agentId: string) => Promise<AgentDetail>;
  /** Poll fetchAgents until agent.ready === true (max 30×500ms = 15s). */
  waitForAgentReady: (agentId: string) => Promise<void>;

  // ── Session actions (write to agents[agentId].*) ──

  fetchSessions: (agentId: string, page?: number) => Promise<void>;
  /** Fetch just the latest session title for the AgentList sidebar (lightweight). */
  fetchLatestSessionTitle: (agentId: string) => Promise<string | null>;
  /** Activate a session on the backend and update frontend state. */
  switchSession: (sessionId: string, agentId?: string) => Promise<void>;
  /** Remember the last selected session for an agent (survives remount). */
  saveSessionForAgent: (agentId: string, sessionId: string) => void;
  createSession: (agentId: string) => Promise<void>;
  deleteSession: (agentId: string, sessionId: string) => Promise<void>;
  closeSession: (agentId: string, sessionId: string) => Promise<void>;
  /** Update a session's title locally (no API call). */
  updateSessionTitle: (sessionId: string, title: string) => void;

  // ── Profile actions ──

  getProfile: (agentId: string) => AgentProfileSettings;
  setProfile: (agentId: string, settings: Partial<AgentProfileSettings>) => void;
  resetProfile: (agentId: string) => void;

  // ── UI actions ──

  setSessionPanelOpen: (open: boolean) => void;
  toggleSessionPanel: () => void;
  /** Reset display-only state on agent switch.
   *  Per-agent storage (agents map) is NOT touched. */
  reset: () => void;
}

// ══════════════════════════════════════════════════════════════════════════
// Store implementation
// ══════════════════════════════════════════════════════════════════════════

export const useAgentStore = create<AgentStoreState>((set, get) => ({
  // ── Initial state ──

  agents: {},
  selectedAgentId: null,
  _userHasSelected: false,
  loading: false,
  error: null,
  isSessionPanelOpen: false,

  // ════════════════════════════════════════════════════════════════════════
  // Agent meta actions
  // ════════════════════════════════════════════════════════════════════════

  fetchAgents: async () => {
    const t0 = performance.now();
    set({ loading: true, error: null });
    try {
      const list = await invoke<AgentInfo[]>("list_agents");
      const t1 = performance.now();
      const sr = list.find((a: AgentInfo) => a.agent_id === "com.acowork.senior-engineer");
      if (sr) {
        console.log(
          `[AgentStore] fetchAgents took ${(t1 - t0).toFixed(0)}ms | senior-engineer: running=${sr.running} ready=${sr.ready} connected=${sr.connected}`,
        );
      }

      // Merge with existing agents map
      const storedProfiles = loadAllProfiles();
      set((state) => {
        const next: Record<string, AgentStorage> = {};
        for (const meta of list) {
          const existing = state.agents[meta.agent_id];
          if (existing) {
            next[meta.agent_id] = { ...existing, meta }; // preserve sessions/profile/etc.
          } else {
            const profile = storedProfiles[meta.agent_id] ?? { ...DEFAULT_PROFILE };
            next[meta.agent_id] = createStorage(meta, profile);
          }
        }

        // Remove agents that no longer exist
        for (const id of Object.keys(state.agents)) {
          if (!next[id]) {
            delete next[id];
          }
        }

        // Auto-select System Agent on initial load
        let selId = state.selectedAgentId;
        if (!selId && !state._userHasSelected && list.length > 0) {
          const sys = list.find((a) => a.agent_id === SYSTEM_AGENT_ID);
          selId = sys ? SYSTEM_AGENT_ID : list[0].agent_id;
        }

        return { agents: next, selectedAgentId: selId, loading: false };
      });
    } catch (e) {
      set({ error: String(e), loading: false });
    }
  },

  selectAgent: (id) => {
    set({ selectedAgentId: id, _userHasSelected: true });
  },

  installAgent: async (packagePath) => {
    try {
      await invoke("install_agent", { packagePath, devMode: true });
      await get().fetchAgents();
    } catch (e) {
      set({ error: String(e) });
      throw e;
    }
  },

  uninstallAgent: async (agentId) => {
    if (agentId === SYSTEM_AGENT_ID) throw new Error("System Agent cannot be uninstalled");
    try {
      // Capture version before removal — needed to clear the avatar blob cache
      const version = get().agents[agentId]?.meta.version;

      await invoke("uninstall_agent", { agentId });

      // Clear avatar blob URL cache so a re-install fetches fresh bytes
      clearAgentAvatarCache(agentId, version);

      // Clean up profile from localStorage
      try {
        const raw = localStorage.getItem(STORAGE_KEY);
        if (raw) {
          const profiles = JSON.parse(raw) as Record<string, unknown>;
          if (profiles[agentId]) {
            delete profiles[agentId];
            localStorage.setItem(STORAGE_KEY, JSON.stringify(profiles));
          }
        }
      } catch {
        // localStorage unavailable — non-fatal
      }

      // Disconnect WebSocket and remove chatStore agent state
      const chatStore = useChatStore.getState();
      chatStore.disconnectStream(agentId);
      useChatStore.setState((state) => {
        const next = { ...state.agentStates };
        delete next[agentId];
        return { agentStates: next };
      });

      set((state) => {
        const next = { ...state.agents };
        delete next[agentId];
        let selId = state.selectedAgentId;
        if (selId === agentId) {
          const remaining = Object.values(next);
          const sys = remaining.find((s) => s.meta.agent_id === SYSTEM_AGENT_ID);
          selId = sys?.meta.agent_id ?? (remaining[0]?.meta.agent_id ?? null);
        }
        return { agents: next, selectedAgentId: selId };
      });
    } catch (e) {
      set({ error: String(e) });
      throw e;
    }
  },

  startAgent: async (agentId, devMode) => {
    try {
      await invoke("start_agent", { agentId, devMode: devMode ?? false });
      await get().fetchAgents();
    } catch (e) {
      set({ error: String(e) });
      throw e;
    }
  },

  stopAgent: async (agentId) => {
    try {
      await invoke("stop_agent", { agentId });
      await get().fetchAgents();
    } catch (e) {
      set({ error: String(e) });
      throw e;
    }
  },

  restartAgentInDebug: async (agentId) => {
    try {
      await invoke("restart_agent_in_debug", { agentId });
      await get().fetchAgents();
    } catch (e) {
      set({ error: String(e) });
      throw e;
    }
  },

  getAgentDetail: async (agentId) => {
    return await invoke<AgentDetail>("get_agent_detail", { agentId });
  },

  waitForAgentReady: async (agentId) => {
    for (let attempt = 0; attempt < 30; attempt++) {
      await get().fetchAgents();
      const storage = get().agents[agentId];
      if (storage?.meta.ready) return;
      if (!storage?.meta.running) {
        throw new Error("Agent process exited before becoming ready");
      }
      await new Promise((resolve) => setTimeout(resolve, 500));
    }
    throw new Error("Agent did not become ready within 15 seconds");
  },

  // ════════════════════════════════════════════════════════════════════════
  // Session actions
  // ════════════════════════════════════════════════════════════════════════

  fetchSessions: async (agentId: string, page?: number) => {
    const requestId = ++fetchSessionReqId;
    const currentPage = page ?? get().agents[agentId]?.pagination.currentPage ?? 1;
    const pageSize = get().agents[agentId]?.pagination.pageSize ?? 20;

    // Set per-agent loading
    set((state) => {
      const existing = state.agents[agentId];
      if (!existing) return state;
      return patchAgent(state, agentId, { isLoading: true });
    });

    try {
      const resp = await fetch(
        `${getGatewayUrl()}/api/agents/${agentId}/sessions?page=${currentPage}&size=${pageSize}`,
      );
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      const data = (await resp.json()) as {
        sessions: SessionInfo[];
        total_count: number;
        total_pages: number;
      };
      const sessions = (data.sessions ?? []).sort(
        (a, b) => new Date(b.created_at).getTime() - new Date(a.created_at).getTime(),
      );
      if (requestId !== fetchSessionReqId) {
        set((state) => patchAgent(state, agentId, { isLoading: false }));
        return; // stale
      }

      const title = sessions.length > 0 ? (sessions[0]?.title ?? "") : null;

      set((state) =>
        patchAgent(state, agentId, {
          sessions,
          isLoading: false,
          sessionTitle: title,
          pagination: {
            currentPage,
            totalPages: data.total_pages ?? 1,
            totalCount: data.total_count ?? 0,
            pageSize,
          },
        }),
      );

      // ADR-014: Pull repair — use backend sessionStatus to correct frontend state
      const chatStore = useChatStore.getState();
      const mismatches = new Map<string, SessionStatus>();
      for (const session of sessions) {
        if (session.status) {
          const sessionState = chatStore.getSessionState(agentId, session.session_id);
          const frontendStatus = sessionState?.sessionStatus;
          if (!frontendStatus) {
            if (isSessionActive(session.status)) {
              mismatches.set(session.session_id, session.status);
            }
          } else {
            const prevStatus = JSON.stringify(frontendStatus);
            const newStatus = JSON.stringify(session.status);
            if (prevStatus !== newStatus) {
              mismatches.set(session.session_id, session.status);
            }
          }
        }
      }
      if (mismatches.size > 0) {
        chatStore.batchUpdateSessionStatuses(agentId, mismatches);
      }

      // Sync session workspaces
      useWorkspaceStore.getState().syncSessionWorkspaces(sessions);
    } catch (e) {
      if (requestId !== fetchSessionReqId) {
        set((state) => patchAgent(state, agentId, { isLoading: false }));
        return;
      }
      console.error("[AgentStore] Failed to fetch sessions:", e);
      set((state) => patchAgent(state, agentId, { sessions: [], isLoading: false }));
    }
  },

  fetchLatestSessionTitle: async (agentId: string) => {
    try {
      const resp = await fetch(
        `${getGatewayUrl()}/api/agents/${agentId}/sessions?page=1&size=1`,
      );
      if (!resp.ok) return null;
      const data = (await resp.json()) as { sessions: SessionInfo[] };
      const session = data.sessions?.[0];
      if (!session) {
        set((state) => patchAgent(state, agentId, { sessionTitle: null }));
        return null;
      }
      const title = session.title ?? "";
      set((state) => patchAgent(state, agentId, { sessionTitle: title }));
      return title;
    } catch {
      return null;
    }
  },

  switchSession: async (sessionId: string, agentId?: string) => {
    if (!agentId) return;
    if (sessionId === useChatStore.getState().getActiveSessionId(agentId)) return;

    useChatStore.getState().activateSession(agentId, sessionId);
    useChatStore.getState().abortSessionLoad();

    try {
      const resp = await fetch(
        `${getGatewayUrl()}/api/agents/${agentId}/sessions/${sessionId}/activate`,
        { method: "POST" },
      );
      if (resp.ok) {
        const meta = (await resp.json()) as {
          session_id: string;
          activated: boolean;
          model?: string | null;
          provider?: string | null;
          workspace_id?: string | null;
        };
        useChatStore.getState().applySessionMeta(agentId, sessionId, meta);
      }
    } catch (e) {
      console.warn("[AgentStore] activate_session failed:", e);
    }

    get().saveSessionForAgent(agentId, sessionId);

    // ADR-014: Pull repair — refresh session statuses on switch
    get().fetchSessions(agentId);
  },

  saveSessionForAgent: (agentId: string, sessionId: string) => {
    set((state) => patchAgent(state, agentId, { rememberedSessionId: sessionId }));
  },

  createSession: async (agentId: string) => {
    try {
      const lastActiveWs =
        useWorkspaceStore
          .getState()
          .workspaces.find((w) => w.last_active)
          ?.id ?? null;

      // model/provider is managed by Runtime internally via
      // SessionManager::current_model_and_provider() fallback.
      // Frontend MUST NOT cache or pass preferredModel/preferredProvider
      // — that violates the display-only principle.
      const body: Record<string, string> = {};
      if (lastActiveWs) body.workspace_id = lastActiveWs;

      const resp = await fetch(`${getGatewayUrl()}/api/agents/${agentId}/sessions`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      });
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      const data = (await resp.json()) as { session_id: string };
      const newSession: SessionInfo = {
        session_id: data.session_id,
        created_at: new Date().toISOString(),
        message_count: 0,
        title: null,
        status: { status: "idle" },
      };

      set((state) => {
        const existing = state.agents[agentId];
        if (!existing) return state;
        return patchAgent(state, agentId, {
          sessions: [newSession, ...existing.sessions],
        });
      });

      if (lastActiveWs) {
        useWorkspaceStore.getState().setSessionWorkspaceLocal(data.session_id, lastActiveWs);
      }

      useChatStore.getState().activateSession(agentId, data.session_id);
    } catch (e) {
      console.error("[AgentStore] Failed to create session:", e);
    }
  },

  closeSession: async (agentId: string, sessionId: string) => {
    try {
      const resp = await fetch(
        `${getGatewayUrl()}/api/agents/${agentId}/sessions/${sessionId}/close`,
        { method: "POST" },
      );
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      await resp.json();

      const storage = get().agents[agentId];
      if (!storage) return;
      const isCurrent = useChatStore.getState().getActiveSessionId(agentId) === sessionId;
      const remaining = storage.sessions.filter((s) => s.session_id !== sessionId);
      const newCurrentId = isCurrent
        ? remaining.length > 0
          ? remaining[0].session_id
          : null
        : useChatStore.getState().getActiveSessionId(agentId);

      set((state) => patchAgent(state, agentId, { sessions: remaining }));

      const openIds = useChatStore.getState().getOpenSessionIds(agentId);
      if (openIds.includes(sessionId)) {
        useChatStore.getState().closeTab(agentId, sessionId);
      }

      if (isCurrent) {
        if (newCurrentId) {
          useChatStore.getState().activateSession(agentId, newCurrentId);
          get().saveSessionForAgent(agentId, newCurrentId);
        } else {
          useChatStore.getState().clearMessages(agentId);
        }
      }
      useChatStore.getState().removeSessionState(agentId, sessionId);
    } catch (e) {
      console.error("[AgentStore] Failed to close session:", e);
    }
  },

  deleteSession: async (agentId: string, sessionId: string) => {
    try {
      const resp = await fetch(
        `${getGatewayUrl()}/api/agents/${agentId}/sessions/${sessionId}`,
        { method: "DELETE" },
      );
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      const data = (await resp.json()) as {
        deleted: boolean;
        session_id: string;
        new_session_id?: string;
      };

      const storage = get().agents[agentId];
      if (!storage) return;
      const isCurrent = useChatStore.getState().getActiveSessionId(agentId) === sessionId;
      const remaining = storage.sessions.filter((s) => s.session_id !== sessionId);
      const newCurrentId = isCurrent
        ? data.new_session_id ||
          (remaining.length > 0 ? remaining[0].session_id : null)
        : useChatStore.getState().getActiveSessionId(agentId);

      set((state) => patchAgent(state, agentId, { sessions: remaining }));

      const openIds = useChatStore.getState().getOpenSessionIds(agentId);
      if (openIds.includes(sessionId)) {
        useChatStore.getState().closeTab(agentId, sessionId);
      }

      if (isCurrent) {
        if (newCurrentId) {
          useChatStore.getState().activateSession(agentId, newCurrentId);
          get().saveSessionForAgent(agentId, newCurrentId);
        } else {
          useChatStore.getState().clearMessages(agentId);
        }
      }
      useChatStore.getState().removeSessionState(agentId, sessionId);

      // Invalidate session title so it gets re-fetched (undefined = not yet fetched)
      set((state) => patchAgent(state, agentId, { sessionTitle: undefined }));
    } catch (e) {
      console.error("[AgentStore] Failed to delete session:", e);
    }
  },

  updateSessionTitle: (sessionId: string, title: string) => {
    set((state) => {
      for (const id of Object.keys(state.agents)) {
        const storage = state.agents[id];
        const idx = storage.sessions.findIndex((s) => s.session_id === sessionId);
        if (idx !== -1) {
          const sessions = [...storage.sessions];
          const existing = sessions[idx];
          if (!existing || (existing.title && existing.title.trim() !== "")) {
            break; // already has a title, skip
          }
          sessions[idx] = { ...existing, title };
          return {
            agents: {
              ...state.agents,
              [id]: { ...storage, sessions },
            },
          };
        }
      }
      return state;
    });
  },

  // ════════════════════════════════════════════════════════════════════════
  // Profile actions
  // ════════════════════════════════════════════════════════════════════════

  getProfile: (agentId) => {
    const storage = get().agents[agentId];
    return storage?.profile ?? { ...DEFAULT_PROFILE };
  },

  setProfile: (agentId, settings) => {
    set((state) => {
      const existing = state.agents[agentId];
      if (!existing) return state;
      const updated: AgentProfileSettings = {
        ...existing.profile,
        ...settings,
      };
      // Persist to localStorage
      const allProfiles = profilesToRecord(state.agents);
      allProfiles[agentId] = updated;
      saveAllProfiles(allProfiles);

      return patchAgent(state, agentId, { profile: updated });
    });
  },

  resetProfile: (agentId) => {
    set((state) => {
      const existing = state.agents[agentId];
      if (!existing) return state;
      const allProfiles = profilesToRecord(state.agents);
      delete allProfiles[agentId];
      saveAllProfiles(allProfiles);

      return patchAgent(state, agentId, { profile: { ...DEFAULT_PROFILE } });
    });
  },

  // ════════════════════════════════════════════════════════════════════════
  // UI actions
  // ════════════════════════════════════════════════════════════════════════

  setSessionPanelOpen: (open) => {
    set({ isSessionPanelOpen: open });
  },

  toggleSessionPanel: () => {
    set((state) => ({ isSessionPanelOpen: !state.isSessionPanelOpen }));
  },

  reset: () => {
    // Cancel any in-flight fetch
    ++fetchSessionReqId;
    // Only reset display state — per-agent storage is indexed by agentId and
    // switching agents must NOT clear it (that would cause sidebar flicker).
    set({ isSessionPanelOpen: false });
  },
}));

// ── Helper ──────────────────────────────────────────────────────────────

function profilesToRecord(storages: Record<string, AgentStorage>): Record<string, AgentProfileSettings> {
  const out: Record<string, AgentProfileSettings> = {};
  for (const [id, s] of Object.entries(storages)) {
    out[id] = s.profile;
  }
  return out;
}
