import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import { BUILTIN_ICON_IDS } from "../components/common/UserAvatar";
import { clearAgentAvatarCache } from "../lib/avatar";
import type { AgentInfo, AgentDetail, SessionInfo, SessionStatus } from "../lib/types";
import { isProcessing } from "../lib/types";
import { getGatewayUrl } from "../lib/config";
import { useChatStore } from "./chatStore";
import { useWorkspaceStore } from "./workspaceStore";
import { log } from "../lib/logger";

/** System Agent ID — always auto-started by Gateway */
export const SYSTEM_AGENT_ID = "com.acowork.system";

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
  maxSessions?: number;
  systemPrompt?: string;
  shellApprovalThreshold?: string;
  approvalTimeoutSecs?: number;
  /** Per-agent LLM temperature override (0.0–2.0).
   *  Undefined = use manifest default or system default (0.3). */
  temperature?: number;
  /** Per-agent context window cap in tokens (0 = no limit).
   *  Undefined = use manifest default or system default (200K). */
  contextWindow?: number;
  globalMaxTokens?: number;
  activeModel?: string;
  activeProvider?: string;
  /** ADR-052: Whether context_retrieve + context_abandon tools are registered.
   *  Undefined = use default (true). Boot-only: takes effect on next session restore. */
  toolCompressionEnabled?: boolean;
  /** Idle (auto-sleep) timeout in seconds before the Runtime self-terminates.
   *  0 = never sleep. Undefined = use manifest default or system default (1800). */
  idleTimeoutSecs?: number;
}

const DEFAULT_PROFILE: AgentProfileSettings = {
  displayName: undefined,
  avatarIconId: null,
  modelId: undefined,
  providerId: undefined,
  maxTokens: 0,
  maxIterations: 0,
  maxSessions: 0,
  systemPrompt: undefined,
  shellApprovalThreshold: undefined,
  approvalTimeoutSecs: undefined,
  toolCompressionEnabled: undefined,
  idleTimeoutSecs: undefined,
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
    maxSessions: typeof s.maxSessions === "number" && s.maxSessions > 0 ? s.maxSessions : 0,
    systemPrompt: s.systemPrompt,
    // Normalize the legacy "never" spelling (pre-rename) to "auto_approve".
    shellApprovalThreshold:
      s.shellApprovalThreshold === "never" ? "auto_approve" : s.shellApprovalThreshold,
    temperature:
      typeof s.temperature === "number" && s.temperature >= 0 && s.temperature <= 2
        ? s.temperature
        : undefined,
    contextWindow:
      typeof s.contextWindow === "number" && s.contextWindow >= 0
        ? s.contextWindow
        : undefined,
    approvalTimeoutSecs:
      typeof s.approvalTimeoutSecs === "number" && s.approvalTimeoutSecs > 0
        ? s.approvalTimeoutSecs
        : undefined,
    globalMaxTokens: typeof s.globalMaxTokens === "number" ? s.globalMaxTokens : undefined,
    activeModel: typeof s.activeModel === "string" ? s.activeModel : undefined,
    activeProvider: typeof s.activeProvider === "string" ? s.activeProvider : undefined,
    toolCompressionEnabled:
      typeof s.toolCompressionEnabled === "boolean"
        ? s.toolCompressionEnabled
        : undefined,
    // idleTimeoutSecs: number >= 0 (0 = never sleep). Undefined = use manifest default.
    idleTimeoutSecs:
      typeof s.idleTimeoutSecs === "number" && s.idleTimeoutSecs >= 0
        ? s.idleTimeoutSecs
        : undefined,
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
  /** ADR-028: agent-scoped cumulative token totals — fallback data source
   *  for the Results Panel when the live `context_usage` WebSocket push
   *  hasn't fired yet (e.g. fresh Runtime with no LLM calls, or session
   *  not yet active). Refreshed on every successful session-list fetch.
   *  `null` = not yet fetched / older Runtime without ADR-028. */
  agentTokenTotals: { input: number; output: number } | null;
  /** Agent online/offline status — updated by agent_status MQTT event.
   *  Defaults to `true` so the first paint shows a "running" agent
   *  rather than a blank/inactive state — the Gateway's `/api/agents`
   *  and the Runtime's first `"online"` retained message will overwrite
   *  this within the first 1-2 s. */
  online: boolean;
  /** Runtime self-reported auto-sleep (idle_watcher fired). True after
   *  the Runtime published `"sleeping"` to the status retained topic
   *  but before the process actually exits (the retained message stays
   *  cached until the Will "offline" overwrites it). Lets the UI render
   *  the sleeping empty state without waiting for a full polling cycle. */
  sleeping: boolean;
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
    agentTokenTotals: null,
    online: true,
    sleeping: false,
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
  /** Fetch the latest session (by last_active_at desc) and persist its title
   *  into `agents[agentId].sessionTitle` so the AgentList sidebar reflects it
   *  without a separate title-only fetch. Returns null if the agent has no
   *  sessions, is not connected, or the Runtime HTTP server is not yet
   *  listening. */
  fetchLatestSession: (agentId: string) => Promise<{ session_id: string; title: string | null } | null>;
  /**
   * Activate a session that has just been created (Runtime has already
   * confirmed via `session_created` event that the session exists and is
   * Active). This is "fast-path" activation: open the UI tab + send the
   * open_session MQTT message + load messages, atomically. Used by the
   * `session_created` event handler so the user's "+ button" lands them on
   * the fresh chat without an intermediate click.
   */
  activateNewlyCreatedSession: (sessionId: string, agentId: string) => Promise<void>;
  /** Remember the last selected session for an agent (survives remount). */
  saveSessionForAgent: (agentId: string, sessionId: string) => void;
  createSession: (agentId: string) => Promise<void>;
  deleteSession: (agentId: string, sessionId: string) => Promise<void>;
  closeSession: (agentId: string, sessionId: string) => Promise<void>;
  /** Update a session's title locally (no API call). */
  updateSessionTitle: (sessionId: string, title: string) => void;

  // ── Agent lifecycle (MQTT-driven) ──

  /** Update agent's online/offline status (from MQTT agent_status event).
   *  `sleeping` is optional for backward compatibility with older callers. */
  updateAgentOnlineStatus: (
    agentId: string,
    online: boolean,
    sleeping?: boolean,
  ) => void;
  /** Patch specific meta fields without a full state reload. */
  patchAgentMeta: (agentId: string, meta: Partial<Pick<AgentInfo, "name" | "version" | "avatar" | "builtin_avatar" | "display_name" | "role">>) => void;

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
        log.debug(
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
            // Fold in `running` from the latest snapshot — if the
            // Runtime auto-slept (or crashed) between MQTT events, the
            // Gateway's `/api/agents` is the only source of truth for
            // `running=false` + `sleeping_at`. We also normalise
            // `online`/`sleeping` here: if the Gateway says
            // `running=false`, force `online=false, sleeping=false`
            // even if the in-memory MQTT event hasn't propagated yet.
            // This prevents the stale "online=true" window where the
            // ChatPanel keeps showing the session input after the
            // Runtime is gone.
            const gateway_says_alive = !!meta.running;
            next[meta.agent_id] = {
              ...existing,
              meta,
              online: gateway_says_alive ? existing.online : false,
              sleeping: gateway_says_alive ? existing.sleeping : false,
            };
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

        // Auto-select: always pick the agent with the largest
        // last_interaction_at.  All agents (including system) are equal —
        // the backend owns the truth, the frontend just displays it.
        // Fallback to list[0] (system per sort order) when nothing
        // has ever been interacted with.
        let selId = state.selectedAgentId;
        if (!selId && list.length > 0) {
          let bestId: string | null = null;
          let bestTs = -1;
          for (const a of list) {
            const ts = a.last_interaction_at ? Date.parse(a.last_interaction_at) : -1;
            if (!Number.isNaN(ts) && ts > bestTs) {
              bestTs = ts;
              bestId = a.agent_id;
            }
          }
          selId = bestId ?? list[0].agent_id;
        }

        return { agents: next, selectedAgentId: selId, loading: false };
      });

      // Trigger atomic session activation for the selected agent.
      // Backend guarantees /latest-session always returns a session_id
      // for every running agent.
      const current = get();
      if (current.selectedAgentId) {
        current.selectAgent(current.selectedAgentId);
      }
    } catch (e) {
      set({ error: String(e), loading: false });
    }
  },

  selectAgent: (id) => {
    if (!id) return;
    set({ selectedAgentId: id });

    // Guard: business endpoints (fetchLatestSession → openSession →
    // fetchSessionState) all 503 against an unregistered Runtime.
    // Unstarted agents are a legitimate UI state — the user picks the
    // Start button (or double-clicks the list item) to launch them, and
    // `startAgentAndSyncUI` atomically bootstraps the session there.
    // Probing here would only produce a 503 + console error for an
    // expected, user-driven transition.
    //
    // ADR-038: `switchSession` was removed; UI-bound session activation
    // now flows through `chatStore.openSession`, which sends the
    // `open_session` MQTT command and reloads messages atomically.
    //
    // Same gate used by ChatPanel's mount effect ("if (!running || !ready)
    // return") so the two paths can never drift out of sync.
    const meta = get().agents[id]?.meta;
    if (!meta?.running || !meta?.ready) return;

    // 原子化：选 agent 时加载 latest session 并激活。
    // openSession 内部会调后端 open_session (拉起 Closed 状态到 Active)、
    // 写入 session 元数据、拉取 session 列表。fetchSessionState 补上 context
    // usage / todos。loadModels 由 ChatPanel 的 useEffect
    // 在 selectedAgentId 变化 + running && ready 时自动触发。
    const chat = useChatStore.getState();
    if (!chat.agentStates[id]?.activeSessionId) {
      get().fetchLatestSession(id).then(async (latest) => {
        if (!latest?.session_id) return;
        // ADR-038: opening from the agent sidebar is a "first-open" scenario,
        // so we use the full openSession (UI + MQTT + load) instead of the
        // strict setActiveTab.
        // ADR-047: openSession now internally calls loadSession (config + state).
        await chat.openSession(id, latest.session_id);
        // Populate the sessions array so the session tab bar and panel
        // display the correct title instead of "Untitled" until the user
        // manually opens the session list (which triggers fetchSessions).
        get().fetchSessions(id);
      });
    }
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
        // ADR-028: optional fallback data source for agent-scoped token
        // totals. Absent on older Runtimes — both fields `undefined`.
        agent_total_input_tokens?: number;
        agent_total_output_tokens?: number;
      };
      const sessions = (data.sessions ?? []).sort(
        (a, b) => new Date(b.created_at).getTime() - new Date(a.created_at).getTime(),
      );
      if (requestId !== fetchSessionReqId) {
        set((state) => patchAgent(state, agentId, { isLoading: false }));
        return; // stale
      }

      const title = sessions.length > 0 ? (sessions[0]?.title ?? "") : null;

      // ADR-028: stash the agent-scoped totals as a fallback data source
      // for the Results Panel. Both fields must be present and finite for
      // the fallback to be usable; otherwise we leave the previous value
      // (or `null` on first fetch) in place.
      const agentTokenTotals =
        typeof data.agent_total_input_tokens === "number" &&
        typeof data.agent_total_output_tokens === "number"
          ? { input: data.agent_total_input_tokens, output: data.agent_total_output_tokens }
          : null;

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
          agentTokenTotals,
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
            if (isProcessing(session.status)) {
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
      log.error("[AgentStore] Failed to fetch sessions:", e);
      set((state) => patchAgent(state, agentId, { sessions: [], isLoading: false }));
    }
  },

  /** Fetch the latest session (by last_active_at desc) without a full disk scan.
   *  The Runtime caches this during startup. Persists the returned title into
   *  `agents[agentId].sessionTitle` so the AgentList sidebar reflects it
   *  without a separate title-only fetch. Returns null if no sessions exist
   *  or the agent is not connected. */
  fetchLatestSession: async (agentId: string) => {
    try {
      const resp = await fetch(
        `${getGatewayUrl()}/api/agents/${agentId}/latest-session`,
      );
      if (!resp.ok) return null;
      const data = (await resp.json()) as {
        session_id: string;
        title: string | null;
        created_at: string | null;
      };
      const title = data.title ?? null;
      // Persist into the sidebar cache. Empty string matches the legacy
      // `fetchLatestSessionTitle` semantics so UI consumers keep working
      // (the AgentList treats `""` and `null` differently: `""` → untitled,
      // `null` → sleep animation). Only running agents reach this branch.
      set((state) => patchAgent(state, agentId, { sessionTitle: title ?? "" }));
      return { session_id: data.session_id, title };
    } catch {
      return null;
    }
  },

  // ADR-038: Activate a session that the Runtime has just confirmed via
  // `session_created`. We delegate to `chatStore.openSession` which owns
  // the full UI+backend+lifecycle transition (UI tab open + MQTT open_session
  // + HTTP messages reload). Idempotent: re-invocations on an already-open
  // session no-op the MQTT side and only refresh the message cache.
  activateNewlyCreatedSession: async (sessionId: string, agentId: string) => {
    // ADR-047: openSession now internally calls loadSession (config + state),
    // so the fresh session's backend `idle` state and config are reflected
    // in the UI before the user types anything.
    await useChatStore.getState().openSession(agentId, sessionId);
    // ADR-014: Refresh the session list so the freshly-created entry is
    // visible in the sidebar/session dropdown.
    get().fetchSessions(agentId);
  },

  saveSessionForAgent: (_agentId: string, _sessionId: string) => {
    // No-op: rememberedSessionId is no longer tracked client-side.
    // The backend /latest-session endpoint is the source of truth
    // for which session is "current", and selectAgent always fetches
    // it fresh on mount / agent switch.
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

      await invoke("mqtt_publish_control", {
        agentId,
        command: "create_session",
        payloadJson: body,
      });

      // NOTE: MQTT create_session does not return a session_id synchronously.
      // The frontend must listen for the `session_created` MQTT event (handled
      // by the Rust backend and forwarded via Tauri event) to obtain the new
      // session_id and proceed with activation.
      // Session meta (workspace_id) will be applied when the session_created
      // event arrives.
    } catch (e) {
      log.error("[AgentStore] Failed to create session:", e);
    }
  },

  closeSession: async (agentId: string, sessionId: string) => {
    try {
      // Close session list entry first; the MQTT `close_session` is fired
      // by `chatStore.closeTab` (which we call below for UI cleanup) — no
      // double-firing.
      const storage = get().agents[agentId];
      if (!storage) return;
      const isCurrent = useChatStore.getState().getActiveSessionId(agentId) === sessionId;
      const remaining = storage.sessions.filter((s) => s.session_id !== sessionId);
      const openIds = useChatStore.getState().getOpenSessionIds(agentId);
      let newCurrentId: string | null;
      if (isCurrent) {
        // Prefer an already-open tab (e.g. the default session) so the user
        // doesn't see a random old session auto-open.  If no open tabs remain,
        // fall back to the first remaining session; if none, clear messages.
        const openRemaining = remaining.filter((s) => openIds.includes(s.session_id));
        if (openRemaining.length > 0) {
          newCurrentId = openRemaining[0].session_id;
        } else if (remaining.length > 0) {
          newCurrentId = remaining[0].session_id;
        } else {
          newCurrentId = null;
        }
      } else {
        newCurrentId = useChatStore.getState().getActiveSessionId(agentId);
      }

      set((state) => patchAgent(state, agentId, { sessions: remaining }));

      if (openIds.includes(sessionId)) {
        // chatStore.closeTab fires MQTT close_session internally and
        // activates the neighbor tab. await it so the new active session
        // is visible before we re-open via openSession below.
        const afterClose = await useChatStore.getState().closeTab(agentId, sessionId);
        if (afterClose && afterClose !== sessionId) {
          newCurrentId = afterClose;
        }
      } else {
        // Session was not in the open-tab strip (probably a closed-background
        // session). Still tell the backend to release its task.
        try {
          await invoke("mqtt_publish_control", {
            agentId,
            command: "close_session",
            payloadJson: { session_id: sessionId },
          });
        } catch (err) {
          log.warn("[AgentStore] close_session MQTT failed:", err);
        }
      }

      if (isCurrent) {
        if (newCurrentId) {
          // ADR-038: re-open the new active tab. openSession is idempotent
          // and ensures the backend has the session Active (it might be
          // a session that was previously closed but never re-opened).
          await useChatStore.getState().openSession(agentId, newCurrentId);
        } else {
          useChatStore.getState().clearMessages(agentId);
        }
      }
      useChatStore.getState().removeSessionState(agentId, sessionId);
    } catch (e) {
      log.error("[AgentStore] Failed to close session:", e);
    }
  },

  deleteSession: async (agentId: string, sessionId: string) => {
    try {
      await invoke("mqtt_publish_control", {
        agentId,
        command: "delete_session",
        payloadJson: { session_id: sessionId },
      });

      const storage = get().agents[agentId];
      if (!storage) return;
      const isCurrent = useChatStore.getState().getActiveSessionId(agentId) === sessionId;
      const remaining = storage.sessions.filter((s) => s.session_id !== sessionId);
      let newCurrentId: string | null = isCurrent
        ? (remaining.length > 0 ? remaining[0].session_id : null)
        : useChatStore.getState().getActiveSessionId(agentId);

      set((state) => patchAgent(state, agentId, { sessions: remaining }));

      const openIds = useChatStore.getState().getOpenSessionIds(agentId);
      if (openIds.includes(sessionId)) {
        const afterClose = await useChatStore.getState().closeTab(agentId, sessionId);
        if (afterClose && afterClose !== sessionId) {
          newCurrentId = afterClose;
        }
      }

      if (isCurrent) {
        if (newCurrentId) {
          // ADR-038: re-open the new active tab. openSession is idempotent
          // and ensures the backend has the session Active (deleting the
          // current session is destructive — the new current may be a
          // Closed session that needs lazy resume).
          await useChatStore.getState().openSession(agentId, newCurrentId);
        } else {
          useChatStore.getState().clearMessages(agentId);
        }
      }
      useChatStore.getState().removeSessionState(agentId, sessionId);

      // Invalidate session title so it gets re-fetched (undefined = not yet fetched)
      set((state) => patchAgent(state, agentId, { sessionTitle: undefined }));
    } catch (e) {
      log.error("[AgentStore] Failed to delete session:", e);
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

  // ── Agent lifecycle (MQTT-driven) ──

  updateAgentOnlineStatus: (
    agentId: string,
    online: boolean,
    sleeping = false,
  ) => {
    // `sleeping` is optional for backward compatibility with callers
    // that only have `online`. The plain-text MQTT status branch
    // (acowork/agents/+/status "online"/"sleeping"/"offline") always
    // passes it; the protobuf branch may omit it in older Runtimes.
    set((state) => patchAgent(state, agentId, { online, sleeping }));
  },

  patchAgentMeta: (agentId: string, meta) => {
    set((state) => {
      const existing = state.agents[agentId];
      if (!existing) return state;
      return patchAgent(state, agentId, {
        meta: { ...existing.meta, ...meta },
      });
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
