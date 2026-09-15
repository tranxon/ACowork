import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import { BUILTIN_ICON_IDS } from "../components/common/UserAvatar";
import { clearAgentAvatarCache } from "../lib/avatar";
import type { AgentInfo, AgentDetail, SessionInfo, SessionStatus, NodeInfo } from "../lib/types";
import { instanceIdOf, isProcessing } from "../lib/types";
import { getGatewayUrl } from "../lib/config";
import { fetchNodes as fetchNodesApi } from "../lib/gateway-api";
import { useChatStore } from "./chatStore";
import { useWorkspaceStore } from "./workspaceStore";
import { useFileTreeStore } from "./fileTree";
import { log } from "../lib/logger";
import { with503Retry } from "../lib/httpRetry";

/** System Agent ID — always auto-started by Gateway */
export const SYSTEM_AGENT_ID = "com.acowork.system";

// ── One-shot "agent came online" waiters (event-driven, no polling) ──────
// startAgent 等待 Runtime 上线：MQTT `agent_status online` → chatStore →
// `updateAgentLiveness(alive=true)` → 触发这里注册的回调。超时由调用方
// (startAgent) 负责 reject。key 为 instance id。
const onlineWaiters = new Map<string, Array<() => void>>();

function notifyAgentOnline(agentId: string): void {
  const waiters = onlineWaiters.get(agentId);
  if (waiters) {
    onlineWaiters.delete(agentId);
    for (const fn of waiters) fn();
  }
}

// ══════════════════════════════════════════════════════════════════════════
// AgentProfile types (moved from agentProfileStore.ts)
// ══════════════════════════════════════════════════════════════════════════

export interface AgentProfileSettings {
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
  /** Idle (auto-sleep) timeout in seconds before the Runtime self-terminates.
   *  0 = never sleep. Undefined = use manifest default or system default (1800). */
  idleTimeoutSecs?: number;
  /** ADR-061: minimum compression ratio for context compaction levels 1-7
   *  (0.05–0.95, expressed as the SAVED share). 0.90 = compress until at
   *  most 10% remains (e.g. 200K → 20K). Undefined = use built-in default (0.90). */
  compressionRatioThreshold?: number;
  /** Per-agent LLM session language override (BCP 47, e.g. `"zh-CN"`,
   *  `"en"`). Undefined = follow the global `UserProfile.language`
   *  (the existing default — Agent Setup panel dropdown default).
   *  Empty string from the dropdown also means "no opinion" and is
   *  cleared on save. */
  sessionLanguage?: string | null;
}

const DEFAULT_PROFILE: AgentProfileSettings = {
  avatarIconId: null,
  modelId: undefined,
  providerId: undefined,
  maxTokens: 0,
  maxIterations: 0,
  maxSessions: 0,
  systemPrompt: undefined,
  shellApprovalThreshold: undefined,
  approvalTimeoutSecs: undefined,
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
    // idleTimeoutSecs: number >= 0 (0 = never sleep). Undefined = use manifest default.
    idleTimeoutSecs:
      typeof s.idleTimeoutSecs === "number" && s.idleTimeoutSecs >= 0
        ? s.idleTimeoutSecs
        : undefined,
    // compressionRatioThreshold: 0.05–0.95 (saved share). Undefined = built-in default.
    compressionRatioThreshold:
      typeof s.compressionRatioThreshold === "number" &&
      s.compressionRatioThreshold >= 0.05 &&
      s.compressionRatioThreshold <= 0.95
        ? s.compressionRatioThreshold
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
   *  `null` = not yet fetched / older Runtime without ADR-028.
   *
   *  ADR-066: widened with `cacheRead` and `cacheWrite` so the status
   *  panel can render cumulative cache-hit / cache-write totals alongside
   *  input / output. Legacy Runtimes (pre-ADR-066) leave the cache fields
   *  at `0`, which the UI treats as "no cache activity reported". */
  agentTokenTotals: {
    input: number;
    output: number;
    cacheRead: number;
    cacheWrite: number;
  } | null;
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
  /** Remote-mode node topology snapshot (ADR-073 §4 sidebar groups,
   *  ADR-059 §6.3 bootstrap refetch). Owned here — not in a component —
   *  so the Gateway-connection lifecycle (drop → `markNodesOffline`,
   *  rise → `fetchNodes`) can drive it from one place. */
  nodes: NodeInfo[];
  /** Global UI state: whether the SessionPanel dropdown is open. (display-only, cleared on agent switch) */
  isSessionPanelOpen: boolean;

  // ── Agent meta actions ──

  fetchAgents: () => Promise<void>;
  /** Refetch the node topology from the Gateway. Failure keeps the
   *  previous snapshot (a transient Gateway blip shouldn't empty the
   *  remote-mode sidebar groups); the rise edge / `bootstrapVersion`
   *  resyncs it. */
  fetchNodes: () => Promise<void>;
  /** Mark every known node offline. Gateway drop edge: the snapshot is
   *  stale the moment the Gateway dies, and no new `bootstrap-state`
   *  snapshot will arrive to bump `bootstrapVersion` (the broker lived
   *  in the Gateway) — without this the sidebar group header keeps
   *  showing a dead node as online. */
  markNodesOffline: () => void;
  selectAgent: (id: string | null) => void;
  installAgent: (packagePath: string, nodeId?: string) => Promise<void>;
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
  /** Rename a session: optimistic local update + MQTT `update_session_title`. */
  renameSession: (agentId: string, sessionId: string, title: string) => Promise<void>;
  /** Update a session's title locally (no API call). */
  updateSessionTitle: (sessionId: string, title: string) => void;

  // ── Agent lifecycle (MQTT-driven) ──

  /** Update agent liveness from the MQTT `agent_status` event
   *  (`online` / `sleeping` / `degraded` / `offline`). Writes the
   *  authoritative `alive` / `sleeping` verdict into `meta` — the single
   *  field every UI consumer gates on. */
  updateAgentLiveness: (
    agentId: string,
    alive: boolean,
    sleeping?: boolean,
  ) => void;
  /** Patch specific meta fields without a full state reload.
   *  `debug_state` is writable because the debug flow (exit DevMode)
   *  may need to align the local cache to the Gateway's confirmed
   *  state when a refresh fails. */
  patchAgentMeta: (agentId: string, meta: Partial<Pick<AgentInfo, "name" | "version" | "avatar" | "builtin_avatar" | "display_name" | "role" | "debug_state">>) => void;

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
  nodes: [],
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
          `[AgentStore] fetchAgents took ${(t1 - t0).toFixed(0)}ms | senior-engineer: alive=${sr.alive} ready=${sr.ready}`,
        );
      }

// Merge with existing agents map
      const storedProfiles = loadAllProfiles();
      set((state) => {
        // ADR-073: the storage map is keyed by INSTANCE identity —
        // the Gateway always supplies `instance_id` on AgentInfo.
        //
        // Empty-list guard: right after a Gateway restart the agent
        // registry may not be populated yet and `list_agents` returns
        // `[]`. That is NOT "all agents uninstalled" — wiping the
        // sidebar here would blank the agent list until the next
        // successful poll (and every rise-edge fetchAgents hits this
        // window). Keep existing entries as-is on an empty list.
        if (list.length === 0 && Object.keys(state.agents).length > 0) {
          return { loading: false };
        }
        const next: Record<string, AgentStorage> = {};
        for (const raw of list) {
          // ADR-048 follow-up: normalise `debug_state` — a Gateway that
          // predates the field omits it, and `undefined` must behave
          // exactly like `"disabled"` for every consumer (Debug Panel
          // gate, "Enable Debug" button, paused banner, settings badge).
          const meta: AgentInfo = {
            ...raw,
            debug_state: raw.debug_state ?? "disabled",
          };
          // ADR-073: the storage map is keyed by INSTANCE identity —
          // the Gateway always supplies `instance_id` on AgentInfo.
          const id = instanceIdOf(meta);
          const existing = state.agents[id];
          if (existing) {
            // `alive` / `sleeping` / `ready` come from the Gateway's
            // authoritative MQTT-registry snapshot — no client-side
            // reconciliation needed. The MQTT `agent_status` handler
            // (updateAgentLiveness) provides the realtime path; this
            // poll is the reconcile fallback that converges any gap.
            next[id] = { ...existing, meta };
          } else {
            // ADR-073: profiles persisted pre-multi-instance are keyed by
            // package id — fall back so existing customisations survive
            // the identity-model upgrade.
            const profile =
              storedProfiles[id] ?? storedProfiles[meta.agent_id] ?? { ...DEFAULT_PROFILE };
            next[id] = createStorage(meta, profile);
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
              bestId = instanceIdOf(a);
            }
          }
          selId = bestId ?? instanceIdOf(list[0]);
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

  fetchNodes: async () => {
    try {
      set({ nodes: await fetchNodesApi() });
    } catch {
      // Gateway unreachable — keep the previous snapshot; the rise edge
      // / `bootstrapVersion` refetch will resync when it's back.
    }
  },

  markNodesOffline: () =>
    set((s) => ({ nodes: s.nodes.map((n) => ({ ...n, online: false })) })),

  selectAgent: (id) => {
    if (!id) return;
    set({ selectedAgentId: id });

    // The file-tree cache is keyed per (agent, workspace) so an old
    // agent's in-flight fetches can never contaminate the new agent's
    // entries. But they WOULD waste bandwidth and could land AFTER the
    // user has already switched back — abort them so the UI only ever
    // waits on the agent it can actually see. This is the call site
    // the fileTreeStore doc comment promised ("called on workspace
    // switch"); the session/workspace switch paths route through
    // selectAgent as well, so a single abort here covers them.
    //
    // `abortAll(id)` KEEPS the newly-selected agent's own in-flight
    // fetches alive — a blanket abort would cancel the tree fetch the
    // UI is currently showing, dropping the node back to `idle` with
    // no re-fetch scheduled (agent home stuck on "Loading…" forever
    // after the first fetchAgents completes and re-selects the same
    // agent, which fires on every 30s list poll).
    useFileTreeStore.getState().abortAll(id);

    // Guard: business endpoints (fetchLatestSession → openSession →
    // fetchSessionState) all 503 against an unregistered Runtime.
    // Unstarted agents are a legitimate UI state — the user picks the
    // Start button (or double-clicks the list item) to launch them, and
    // `startAgentAndSyncUI` atomically bootstraps the session there.
    //
    // Bug B v3 fix: the ready flag is intentionally NOT part of this
    // gate. It is pushed via MQTT retained and arrives asynchronously
    // to Runtime HTTP readiness. Previously, a selectAgent that landed
    // before the MQTT event could never re-fire even after the Runtime
    // port was up — the gate had latched false. Now we gate only on
    // `running` (the user-driven start transition) and let
    // `fetchLatestSession`/`openSession` ride out transient 503s via
    // `with503Retry` (see lib/httpRetry.ts).
    //
    // ADR-038: `switchSession` was removed; UI-bound session activation
    // now flows through `chatStore.openSession`, which sends the
    // `open_session` MQTT command and reloads messages atomically.
    //
    // Same gate used by ChatPanel's mount effect ("if (!alive) return")
    // — ready is no longer required because the with503Retry loop in
    // the data fetchers handles transient 503s during the boot window.
    const meta = get().agents[id]?.meta;
    if (!meta?.alive) return;

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

  installAgent: async (packagePath, nodeId) => {
    try {
      await invoke("install_agent", { packagePath, devMode: true, nodeId: nodeId ?? null });
      await get().fetchAgents();
    } catch (e) {
      set({ error: String(e) });
      throw e;
    }
  },

  uninstallAgent: async (agentId) => {
    // ADR-073: `agentId` here is the INSTANCE id; guard the system
    // package by its manifest identity (the system agent's instance id
    // is a UUID and never equals SYSTEM_AGENT_ID once multi-instance
    // is live).
    if (get().agents[agentId]?.meta.agent_id === SYSTEM_AGENT_ID) {
      throw new Error("System Agent cannot be uninstalled");
    }
    try {
      // Capture version before removal — needed to clear the avatar blob cache
      const version = get().agents[agentId]?.meta.version;

      await invoke("uninstall_agent", { agentId });

      // Clear avatar blob URL cache so a re-install fetches fresh bytes
      clearAgentAvatarCache(agentId, version);

      // Clean up profile from localStorage (keyed by instance id, with
      // legacy package-id fallback).
      try {
        const raw = localStorage.getItem(STORAGE_KEY);
        if (raw) {
          const profiles = JSON.parse(raw) as Record<string, unknown>;
          const meta = get().agents[agentId]?.meta;
          const profileKeys = new Set<string>([agentId]);
          if (meta?.agent_id) profileKeys.add(meta.agent_id);
          let changed = false;
          for (const k of profileKeys) {
            if (profiles[k]) {
              delete profiles[k];
              changed = true;
            }
          }
          if (changed) localStorage.setItem(STORAGE_KEY, JSON.stringify(profiles));
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
          selId = sys ? instanceIdOf(sys.meta) : (remaining[0] ? instanceIdOf(remaining[0].meta) : null);
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
      // Gateway /start 只等到 Node 接受控制命令就返回（node_control
      // start_agent + check_reply）；Runtime 真正上线（MQTT online →
      // updateAgentLiveness(alive=true)）还要 1-3s。这里等"上线事件"
      // 而不是轮询状态：`notifyAgentOnline` 由 agent_status 事件触发，
      // 15s 内没等到即启动失败。若事件先到（invoke 返回时已上线），
      // 直接通过。
      const WAIT_ONLINE_MS = 15_000;
      if (get().agents[agentId]?.meta.alive) return;
      await new Promise<void>((resolve, reject) => {
        const onOnline = (): void => {
          clearTimeout(timer);
          resolve();
        };
        const timer = setTimeout(() => {
          const list = onlineWaiters.get(agentId);
          if (list) {
            const idx = list.indexOf(onOnline);
            if (idx >= 0) list.splice(idx, 1);
            if (list.length === 0) onlineWaiters.delete(agentId);
          }
          reject(new Error(`Agent ${agentId} did not come online within ${WAIT_ONLINE_MS / 1000}s of start`));
        }, WAIT_ONLINE_MS);
        const list = onlineWaiters.get(agentId);
        if (list) list.push(onOnline);
        else onlineWaiters.set(agentId, [onOnline]);
      });
    } catch (e) {
      set({ error: String(e) });
      throw e;
    }
  },

  stopAgent: async (agentId) => {
    try {
      await invoke("stop_agent", { agentId });
      // Drop the cached session runtime state for this agent so the
      // attachment blobs / pending approvals / tool progress lose
      // their refs and can be GC'd. Same cleanup the Gateway-disconnect
      // path uses; without it, stop leaves a stale chat-store footprint
      // behind even though the Runtime is gone.
      useChatStore.getState().clearAgentSessions(agentId);
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
      // ponytail: diagnostic
      const __w0 = performance.now();
      await get().fetchAgents();
      const storage = get().agents[agentId];
      // ponytail: diagnostic
      console.warn(
        `[agentStore] waitForAgentReady attempt=${attempt} ` +
          `after ${Math.round(performance.now() - __w0)}ms ` +
          `alive=${storage?.meta.alive} ready=${storage?.meta.ready}`,
      );
      if (storage?.meta.ready) return;
      if (!storage?.meta.alive) {
        throw new Error("Agent is no longer alive before becoming ready");
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
        // ADR-066: cache totals may be present alongside, also optional.
        agent_total_input_tokens?: number;
        agent_total_output_tokens?: number;
        agent_total_cache_read_tokens?: number;
        agent_total_cache_write_tokens?: number;
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
      //
      // ADR-066: cache totals follow the same gating — if a legacy Runtime
      // omits `agent_total_cache_*`, default both to `0` so the status
      // panel renders "no cache activity reported" rather than `NaN`.
      const agentTokenTotals =
        typeof data.agent_total_input_tokens === "number" &&
        typeof data.agent_total_output_tokens === "number"
          ? {
              input: data.agent_total_input_tokens,
              output: data.agent_total_output_tokens,
              cacheRead: data.agent_total_cache_read_tokens ?? 0,
              cacheWrite: data.agent_total_cache_write_tokens ?? 0,
            }
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
      // Bug B v3 fix: this endpoint proxies through the Runtime and
      // 503s during the window between Gateway discovering the agent
      // and the Runtime HTTP port being registered in the reverse
      // proxy. `with503Retry` honours the Gateway's Retry-After
      // header so a transient 503 at session-switch time recovers
      // transparently without the UI having to retry manually.
      const resp = await with503Retry(
        () => fetch(`${getGatewayUrl()}/api/agents/${agentId}/latest-session`),
        { tag: `AgentStore.fetchLatestSession(${agentId})`, logger: log },
      );
      if (!resp.ok) {
        // ponytail: diagnostic — 404 here is the startup-window race.
        console.warn(
          `[AgentStore] fetchLatestSession(${agentId}) HTTP ${resp.status} @${performance.now().toFixed(0)}ms`,
        );
        return null;
      }
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
    } catch (e) {
      log.error(`[AgentStore] fetchLatestSession(${agentId}) failed:`, e);
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
        instanceId: agentId,
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
            instanceId: agentId,
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
        instanceId: agentId,
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

  renameSession: async (agentId: string, sessionId: string, title: string) => {
    // Optimistic local update — the tab strip, session dropdown, and
    // AgentList sidebar all read `agents[agentId].sessions`, so patch it
    // before the MQTT roundtrip. The Runtime persists the same title via
    // `ConversationSession::update_title_force`.
    set((state) => {
      const storage = state.agents[agentId];
      if (!storage) return state;
      const idx = storage.sessions.findIndex((s) => s.session_id === sessionId);
      if (idx === -1) return state;
      const sessions = [...storage.sessions];
      sessions[idx] = { ...sessions[idx], title };
      return patchAgent(state, agentId, {
        sessions,
        // Keep the sidebar's "latest session title" in sync when the renamed
        // session is the most recently created one (same heuristic used by
        // fetchSessions when it derives `sessionTitle` from `sessions[0]`).
        ...(idx === 0 ? { sessionTitle: title } : {}),
      });
    });

    try {
      await invoke("mqtt_publish_control", {
        instanceId: agentId,
        command: "update_session_title",
        payloadJson: { session_id: sessionId, title },
      });
    } catch (e) {
      log.error("[AgentStore] Failed to rename session:", e);
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
          // Mirror `renameSession`: keep the sidebar's `sessionTitle` in sync
          // when the updated session is the most recently created one
          // (same heuristic `fetchSessions` uses to derive it from
          // `sessions[0]`). Without this, the AgentList keeps showing
          // "Untitled" until the next `fetchSessions` round-trip.
          return patchAgent(state, id, {
            sessions,
            ...(idx === 0 ? { sessionTitle: title } : {}),
          });
        }
      }
      return state;
    });
  },

  // ── Agent lifecycle (MQTT-driven) ──

  updateAgentLiveness: (
    agentId: string,
    alive: boolean,
    sleeping = false,
  ) => {
    // `alive` is the network-level verdict from `acowork/agents/{id}/status`
    // (`online` / `sleeping` / `degraded` → alive; `offline` → not).
    // `sleeping` rides along from the `sleeping` payload. Patch `meta`
    // because that is the single field every UI consumer reads.
    set((state) => {
      const existing = state.agents[agentId];
      if (!existing) return state;
      return patchAgent(state, agentId, {
        meta: { ...existing.meta, alive, sleeping },
      });
    });
    // One-shot online event for startAgent's waiter (no polling).
    if (alive) notifyAgentOnline(agentId);
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
