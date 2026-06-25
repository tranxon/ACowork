import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import type { ChatMessage, ContextUsageInfo, TokenUsage, ToolApprovalNeededEvent, PaginatedMessages, ConversationEntry, SessionStatus, AskQuestionEvent, ModelEntry, TodoItem } from "../lib/types";
import { useAgentStore } from "./agentStore";
import { useUserProfileStore } from "./userProfileStore";
import { useWorkspaceStore } from "./workspaceStore";
import { getGatewayUrl } from "../lib/config";
import i18n from "../i18n";

// ── Sender info helpers ────────────────────────────────────────────────

function getAgentSenderInfo(agentId: string): { senderDisplayName?: string; senderRole?: string } {
  const store = useAgentStore.getState();
  const agentProfile = store.getProfile(agentId);
  const agent = store.agents[agentId]?.meta;
  return {
    senderDisplayName: agentProfile?.displayName ?? agent?.display_name ?? agent?.name,
    senderRole: agent?.role,
  };
}

function getUserSenderInfo(): { senderDisplayName?: string } {
  try {
    const profile = useUserProfileStore.getState().profile;
    return { senderDisplayName: profile.displayName };
  } catch {
    return { senderDisplayName: i18n.t("common.me") };
  }
}

// ---------------------------------------------------------------------------
// Per-session chat state — each session owns an independent instance
// ---------------------------------------------------------------------------

/** State for a single conversation session within an agent. */
interface SessionChatState {
  messages: ChatMessage[];
  streamingMessageId: string | null;
  streamBuffer: string;
  thinkingMessageId: string | null;
  isInThinkPhase: boolean;
  currentTurnId: string | null;
  tokenUsage: TokenUsage | null;
  contextUsage: ContextUsageInfo | null;
  hasMoreMessages: boolean;
  messageCursor: string | null;
  iterationLimitPaused: { iteration: number; maxIterations: number; message: string } | null;
  /** 429 retry wait info — populated from session_state_changed when the provider is rate-limited */
  retryWaitInfo: {
    waitMs: number;
    attempt: number;
    maxAttempts: number;
    provider: string;
    startedAt: number; // Date.now() for frontend countdown timer
  } | null;
  pendingApproval: Record<string, ToolApprovalNeededEvent>;
  pendingQuestion: AskQuestionEvent | null;
  isLoadingSession: boolean;
  loadError: string | null;
  isReasoning: boolean;
  /** ADR-014: Session lifecycle status from backend (source of truth) */
  sessionStatus: SessionStatus | null;
  /** Frontend optimistic flag: true between user clicking Send and backend pushing
   *  session_state_changed. Cleared when sessionStatus arrives or on done/error/stopped. */
  pendingSend: boolean;
  /** Frontend optimistic flag: true between user clicking Stop and backend
   *  pushing stopped/session_state_changed. When true, `sending` is false —
   *  the UI immediately exits the "working" state without waiting for backend. */
  isStopping: boolean;
  /** Last accessed timestamp — used for LRU eviction */
  lastAccessed: number;
  /** Per-session todo list (from todo_write tool) */
  todos: TodoItem[];
  /** Per-session selected model */
  model: string | null;
  /** Per-session selected provider */
  provider: string | null;
  /** Current model chars/token ratio from API calibration */
  ratio: number | null;
  /** Per-session reasoning effort override (frontend display only, source of truth is Runtime) */
  reasoningEffort: string | null;
  /** Per-session temperature override (from Runtime, persisted in JSONL metadata) */
  temperature: number | null;
  /** Context compaction in progress (both manual and auto triggers) */
  isCompacting: boolean;
  /** File tree expanded directory paths (persisted per-session) */
  treeExpandedPaths: string[];
  /** Files/directories/selection attached to chat context (persistent until manually removed) */
  attachedContext: Array<{
    id: string;
    type: "file" | "directory" | "selection";
    name: string;
    absPath: string;
    /** Line range for selection type (1-based, inclusive) */
    startLine?: number;
    endLine?: number;
  }>;
}

const DEFAULT_SESSION_STATE: SessionChatState = {
  messages: [],
  streamingMessageId: null,
  streamBuffer: "",
  thinkingMessageId: null,
  isInThinkPhase: false,
  currentTurnId: null,
  tokenUsage: null,
  contextUsage: null,
  hasMoreMessages: false,
  messageCursor: null,
  iterationLimitPaused: null,
  retryWaitInfo: null,
  pendingApproval: {},
  pendingQuestion: null,
  isLoadingSession: false,
  loadError: null,
  isReasoning: false,
  sessionStatus: null,
  pendingSend: false,
  isStopping: false,
  lastAccessed: 0,
  todos: [],
  model: null,
  provider: null,
  ratio: null,
  reasoningEffort: null,
  temperature: null,
  isCompacting: false,
  treeExpandedPaths: [],
  attachedContext: [],
};

// ---------------------------------------------------------------------------
// Per-agent state — owns session states, WebSocket, model info
// ---------------------------------------------------------------------------

/** State for a single agent — contains all session states + agent-level resources. */
interface AgentState {
  /** Per-session chat states — the core of session isolation */
  sessionStates: Record<string, SessionChatState>;
  /** Currently active session ID for this agent */
  activeSessionId: string | null;
  /** ADR-015: All session IDs that are open as tabs (ordered, max 32) */
  openSessionIds: string[];
  /** Reconnect attempts counter */
  reconnectAttempts: number;
  /** Reconnect timer reference */
  reconnectTimer: ReturnType<typeof setTimeout> | null;
  /** Last loaded session ID — prevents redundant reload */
  lastLoadedSessionId: string | null;
  /** Session init in progress */
  isSessionInitLoading: boolean;
  /** ADR-012: Agent's preferred model — set on every model_switch, inherited by new sessions */
  preferredModel: string | null;
  /** ADR-012: Agent's preferred provider */
  preferredProvider: string | null;
}

const DEFAULT_AGENT_STATE: AgentState = {
  sessionStates: {},
  activeSessionId: null,
  openSessionIds: [],
  reconnectAttempts: 0,
  reconnectTimer: null,
  lastLoadedSessionId: null,
  isSessionInitLoading: false,
  preferredModel: null,
  preferredProvider: null,
};

const MAX_CACHED_SESSIONS = 32;
const MAX_OPEN_TABS = 32;

// ---------------------------------------------------------------------------
// Helper functions for state access
// ---------------------------------------------------------------------------

function getAgentState(state: ChatStore, agentId: string): AgentState {
  return state.agentStates[agentId] ?? DEFAULT_AGENT_STATE;
}

function getSessionState(state: ChatStore, agentId: string, sessionId: string): SessionChatState {
  const agent = state.agentStates[agentId];
  if (!agent) return DEFAULT_SESSION_STATE;
  return agent.sessionStates[sessionId] ?? DEFAULT_SESSION_STATE;
}

/** Get the active session's state for an agent (for backward-compatible reads) */
function getActiveSessionState(state: ChatStore, agentId: string): SessionChatState {
  const agent = getAgentState(state, agentId);
  if (!agent.activeSessionId) return DEFAULT_SESSION_STATE;
  return agent.sessionStates[agent.activeSessionId] ?? DEFAULT_SESSION_STATE;
}

/** Build initial session state, inheriting agent's preferred model (ADR-012). */
function makeInitialSessionState(agent: AgentState): SessionChatState {
  return {
    ...DEFAULT_SESSION_STATE,
    model: agent.preferredModel,
    provider: agent.preferredProvider,
  };
}

/** Produce a new agentStates patch that merges `patch` into the agent's current state */
function updateAgentState(
  state: ChatStore,
  agentId: string,
  patch: Partial<AgentState>,
): { agentStates: Record<string, AgentState> } {
  const current = getAgentState(state, agentId);
  return {
    agentStates: {
      ...state.agentStates,
      [agentId]: { ...current, ...patch },
    },
  };
}

/** Produce a new agentStates patch that merges `patch` into a specific session's state */
function updateSessionState(
  state: ChatStore,
  agentId: string,
  sessionId: string,
  patch: Partial<SessionChatState>,
): { agentStates: Record<string, AgentState> } {
  const agent = getAgentState(state, agentId);
  const currentSession = agent.sessionStates[sessionId] ?? DEFAULT_SESSION_STATE;
  return {
    agentStates: {
      ...state.agentStates,
      [agentId]: {
        ...agent,
        sessionStates: {
          ...agent.sessionStates,
          [sessionId]: { ...currentSession, ...patch, lastAccessed: Date.now() },
        },
      },
    },
  };
}

/** Check whether a session is in an "active" (non-idle) state — used for deriving sending. */
function isSessionSending(session: SessionChatState): boolean {
  return session.pendingSend
    || session.sessionStatus?.status === "streaming"
    || session.sessionStatus?.status === "waiting_approval"
    || session.sessionStatus?.status === "paused";
}

/** Evict oldest/unused sessions when cache exceeds MAX_CACHED_SESSIONS */
function evictStaleSessions(
  state: ChatStore,
  agentId: string,
  protectSessionId?: string,
): { agentStates: Record<string, AgentState> } {
  const agent = getAgentState(state, agentId);
  const sessionIds = Object.keys(agent.sessionStates);
  if (sessionIds.length <= MAX_CACHED_SESSIONS) return { agentStates: state.agentStates };

  // Sort by lastAccessed ascending (oldest first)
  const sorted = sessionIds.sort((a, b) =>
    (agent.sessionStates[a]?.lastAccessed ?? 0) - (agent.sessionStates[b]?.lastAccessed ?? 0)
  );

  const toEvict = sorted
    .filter((id) => !agent.openSessionIds.includes(id) && id !== protectSessionId)
    .slice(0, sessionIds.length - MAX_CACHED_SESSIONS);

  if (toEvict.length === 0) return { agentStates: state.agentStates };

  const newSessionStates = { ...agent.sessionStates };
  for (const id of toEvict) {
    delete newSessionStates[id];
  }

  return {
    agentStates: {
      ...state.agentStates,
      [agentId]: { ...agent, sessionStates: newSessionStates },
    },
  };
}

// ---------------------------------------------------------------------------
// ChatStore — global fields + per-agent agentStates
// ---------------------------------------------------------------------------

interface ChatStore {
  agentStates: Record<string, AgentState>;

  // ---- Global fields (not per-agent) ----
  /** Per-agent WebSocket connections: agentId → WebSocket */
  wsMap: Record<string, WebSocket>;
  availableModels: ModelEntry[];
  /** Whether more messages are being loaded */
  isLoadingMore: boolean;
  /** Load sequence number to prevent race conditions on fast session switches */
  loadSequence: number;
  /** AbortController for cancelling in-flight loadSessionMessages requests */
  abortController: AbortController | null;
  /** Tracks which session titles have already been persisted to backend */
  persistedTitles: Set<string>;

  // ---- Actions ----
  connectStream: (agentId: string, gatewayUrl: string) => void;
  sendMessage: (content: string, agentId: string, command?: string, documentIds?: string[], documents?: Array<{ id: string; filename: string; format: string; size: number; path?: string }>, imageParts?: Array<{ url: string; width: number; height: number }>) => Promise<void>;
  stopCurrentMessage: (agentId: string) => Promise<void>;
  sendStop: (agentId: string) => void;
  disconnectStream: (agentId?: string) => void;
  /** Clear session state for a specific agent's active session */
  clearMessages: (agentId: string) => void;
  /** Clear a specific session's state */
  clearSessionState: (agentId: string, sessionId: string) => void;
  /** Remove a session's cached state (e.g. on session delete) */
  removeSessionState: (agentId: string, sessionId: string) => void;
  trimMessagesTo: (agentId: string, count: number) => void;
  setCurrentModel: (model: string, provider: string, agentId: string) => void;
  setAvailableModels: (models: ModelEntry[]) => void;
  /** Set per-session reasoning effort override (auto/off/low/medium/high) */
  setReasoningEffort: (effort: string, agentId: string) => void;
  getWs: (agentId: string) => WebSocket | undefined;
  continueExecution: (agentId: string) => Promise<void>;
  resolveApproval: (agentId: string) => void;
  /** Resolve a specific approval by tool_call_id, removing it from the pending map. */
  resolveApprovalByToolCallId: (agentId: string, toolCallId: string) => void;
  resolveQuestion: (agentId: string) => void;
  loadConversationHistory: (agentId: string) => Promise<void>;
  loadSessionMessages: (
    agentId: string,
    sessionId: string,
    cursor?: string,
    limit?: number,
    direction?: string,
  ) => Promise<void>;
  abortSessionLoad: () => void;
  loadMoreMessages: (agentId: string, sessionId: string) => Promise<void>;
  /** Activate a session — sets activeSessionId and triggers cleanup */
  activateSession: (agentId: string, sessionId: string) => void;
  /** Apply session metadata (model/provider/workspace_id) from activate_session response */
  applySessionMeta: (agentId: string, sessionId: string, meta: { model?: string | null; provider?: string | null; workspace_id?: string | null }) => void;
  /** Get the active session ID for an agent */
  getActiveSessionId: (agentId: string) => string | null;
  /** ADR-014: Get session state for reading from external stores */
  getSessionState: (agentId: string, sessionId: string) => SessionChatState;
  /** ADR-014: Update session status from backend (Pull repair) */
  updateSessionStatus: (agentId: string, sessionId: string, status: SessionStatus) => void;
  /** ADR-014: Batch update session statuses — single set() call to avoid O(n) re-renders */
  batchUpdateSessionStatuses: (agentId: string, statuses: Map<string, SessionStatus>) => void;
  /** ADR-015: Open a session tab (append to openSessionIds) */
  openTab: (agentId: string, sessionId: string) => void;
  /** ADR-015: Close a session tab (remove from openSessionIds, activate neighbor) */
  closeTab: (agentId: string, sessionId: string) => string | null;
  /** ADR-015: Get open session IDs for an agent */
  getOpenSessionIds: (agentId: string) => string[];
  /** Trigger context compaction for the current session */
  compactContext: (agentId: string, sessionId: string) => void;
  /** Toggle a file tree directory expansion (per-session) */
  toggleTreeExpandedPath: (agentId: string, sessionId: string, relPath: string) => void;
  /** Add a file/directory/selection to attached chat context */
  addAttachedContext: (agentId: string, sessionId: string, item: { id: string; type: "file" | "directory" | "selection"; name: string; absPath: string; startLine?: number; endLine?: number }) => void;
  /** Remove a file/directory from attached chat context */
  removeAttachedContext: (agentId: string, sessionId: string, id: string) => void;
  /** Clear all attached chat context for a session */
  clearAttachedContext: (agentId: string, sessionId: string) => void;
  /** ADR-015 Phase 5: Pull initial session state from backend (model/provider/status/ratio/etc.) */
  fetchSessionState: (agentId: string, sessionId: string) => Promise<void>;
}

function toWsUrl(httpUrl: string, agentId: string): string {
  return `${httpUrl.replace("http://", "ws://").replace("https://", "wss://")}/api/agents/${agentId}/stream`;
}

const MAX_RECONNECT_ATTEMPTS = 10;
const RECONNECT_BASE_MS = 1000;
const RECONNECT_MAX_MS = 30000;

function scheduleReconnect(agentId: string, gatewayUrl: string) {
  const store = useChatStore.getState();
  const agent = getAgentState(store, agentId);
  if (agent.reconnectTimer) return;
  if (agent.reconnectAttempts >= MAX_RECONNECT_ATTEMPTS) {
    console.warn(`[ChatStore] Max reconnect attempts reached for agent ${agentId}, giving up`);
    return;
  }
  const delay = Math.min(RECONNECT_BASE_MS * Math.pow(1.5, agent.reconnectAttempts), RECONNECT_MAX_MS);
  const newAttempts = agent.reconnectAttempts + 1;
  console.log(`[ChatStore] Reconnecting agent ${agentId} in ${Math.round(delay)}ms (attempt ${newAttempts}/${MAX_RECONNECT_ATTEMPTS})`);
  const timer = setTimeout(() => {
    // Clear timer ref first
    useChatStore.setState((state) => updateAgentState(state, agentId, { reconnectTimer: null }));
    const currentStore = useChatStore.getState();
    if (!currentStore.wsMap[agentId]) {
      currentStore.connectStream(agentId, gatewayUrl);
    }
  }, delay);
  useChatStore.setState((state) =>
    updateAgentState(state, agentId, { reconnectTimer: timer, reconnectAttempts: newAttempts })
  );
}

function resetReconnect(agentId: string) {
  const store = useChatStore.getState();
  const agent = getAgentState(store, agentId);
  if (agent.reconnectTimer) {
    clearTimeout(agent.reconnectTimer);
  }
  useChatStore.setState((state) =>
    updateAgentState(state, agentId, { reconnectTimer: null, reconnectAttempts: 0 })
  );
}

function resetAllReconnects() {
  const store = useChatStore.getState();
  for (const agentId of Object.keys(store.agentStates)) {
    const agent = store.agentStates[agentId];
    if (agent.reconnectTimer) clearTimeout(agent.reconnectTimer);
  }
  // Batch reset all agents' reconnect state
  const newAgentStates: Record<string, AgentState> = {};
  for (const [id, agent] of Object.entries(store.agentStates)) {
    newAgentStates[id] = { ...agent, reconnectTimer: null, reconnectAttempts: 0 };
  }
  useChatStore.setState({ agentStates: newAgentStates });
}

export const useChatStore = create<ChatStore>((set, get) => ({
  agentStates: {},
  wsMap: {},
  availableModels: [],
  isLoadingMore: false,
  loadSequence: 0,
  abortController: null,
  persistedTitles: new Set(),

  getWs: (agentId: string) => get().wsMap[agentId],

  getActiveSessionId: (agentId: string) => {
    return getAgentState(get(), agentId).activeSessionId;
  },

  // ADR-014: Get session state for reading from external stores
  getSessionState: (agentId: string, sessionId: string): SessionChatState => {
    return getSessionState(get(), agentId, sessionId);
  },

  // ADR-014: Update session status from backend (Pull repair)
  // Also creates SessionChatState entry if not cached (e.g. crash restart)
  updateSessionStatus: (agentId: string, sessionId: string, status: SessionStatus) => {
    set((state) => {
      const agent = getAgentState(state, agentId);
      const session = agent.sessionStates[sessionId];
      if (!session) {
        // Crash restart: create entry with backend status
        const updatedSessions = { ...agent.sessionStates, [sessionId]: { ...makeInitialSessionState(agent), sessionStatus: status, pendingSend: false, lastAccessed: Date.now() } };
        const updatedAgent = { ...agent, sessionStates: updatedSessions };
        return { agentStates: { ...state.agentStates, [agentId]: updatedAgent } };
      }
      // Clear pendingSend when backend status arrives
      return updateSessionState(state, agentId, sessionId, { sessionStatus: status, pendingSend: false });
    });
  },

  // ADR-014: Batch update — single set() call, O(1) re-render regardless of session count
  // Also creates SessionChatState entries for sessions not yet cached (e.g. crash restart)
  batchUpdateSessionStatuses: (agentId: string, statuses: Map<string, SessionStatus>) => {
    if (statuses.size === 0) return;
    set((state) => {
      const agent = getAgentState(state, agentId);
      const updatedSessions = { ...agent.sessionStates };
      for (const [sessionId, status] of statuses) {
        const session = updatedSessions[sessionId];
        if (session) {
          // Clear pendingSend when backend status arrives
          updatedSessions[sessionId] = { ...session, sessionStatus: status, pendingSend: false, lastAccessed: Date.now() };
        } else {
          // Crash restart: session not cached yet — create entry with backend status
          updatedSessions[sessionId] = {
            ...makeInitialSessionState(agent),
            sessionStatus: status,
            pendingSend: false,
            lastAccessed: Date.now(),
          };
        }
      }
      const updatedAgent = { ...agent, sessionStates: updatedSessions };
      return { agentStates: { ...state.agentStates, [agentId]: updatedAgent } };
    });
  },

  // ADR-015: Open a session as a tab
  openTab: (agentId: string, sessionId: string) => {
    set((state) => {
      const agent = getAgentState(state, agentId);
      if (agent.openSessionIds.includes(sessionId)) {
        // Already open — just activate it
        return updateAgentState(state, agentId, { activeSessionId: sessionId });
      }
      // Append to end, cap at MAX_OPEN_TABS
      const newOpenIds = [...agent.openSessionIds, sessionId].slice(-MAX_OPEN_TABS);
      return updateAgentState(state, agentId, { openSessionIds: newOpenIds, activeSessionId: sessionId });
    });
  },

  // ADR-015: Close a session tab, returns the new active sessionId (or null)
  closeTab: (agentId: string, sessionId: string): string | null => {
    let newActiveId: string | null = null;
    set((state) => {
      const agent = getAgentState(state, agentId);
      const idx = agent.openSessionIds.indexOf(sessionId);
      if (idx === -1) return {}; // Not open

      const newOpenIds = agent.openSessionIds.filter((id) => id !== sessionId);

      // If closing the active tab, activate neighbor
      if (agent.activeSessionId === sessionId) {
        // Prefer right neighbor, then left
        const neighborIdx = Math.min(idx, newOpenIds.length - 1);
        newActiveId = newOpenIds[neighborIdx] ?? null;
      } else {
        newActiveId = agent.activeSessionId;
      }

      return updateAgentState(state, agentId, {
        openSessionIds: newOpenIds,
        activeSessionId: newActiveId,
      });
    });
    return newActiveId;
  },

  // ADR-015: Get open session IDs for reading
  getOpenSessionIds: (agentId: string): string[] => {
    return getAgentState(get(), agentId).openSessionIds;
  },

  /** Trigger context compaction for the current session (manual trigger).
   *  Sends compact_context WS message and sets optimistic isCompacting flag.
   *  The backend emits CompactingStarted → compacting_started → isCompacting = true
   *  When compaction completes, context_usage event clears isCompacting. */
  compactContext: (agentId: string, sessionId: string) => {
    const ws = get().wsMap[agentId];
    if (ws && ws.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify({ type: "compact_context", session_id: sessionId }));
      set((state) => updateSessionState(state, agentId, sessionId, { isCompacting: true }));
    }
  },

  activateSession: (agentId: string, sessionId: string) => {
    set((state) => {
      const agent = getAgentState(state, agentId);
      // No-op if already active
      if (agent.activeSessionId === sessionId) return {};

      const patches: Partial<AgentState> = { activeSessionId: sessionId };

      // ADR-015: Ensure session is in openSessionIds (open tab)
      if (!agent.openSessionIds.includes(sessionId)) {
        const newOpenIds = [...agent.openSessionIds, sessionId].slice(-MAX_OPEN_TABS);
        patches.openSessionIds = newOpenIds;
      }

      let newSessionStates = { ...agent.sessionStates };

      // NOTE: We do NOT clear the old session's transient state (streaming, thinking, etc.)
      // because the agent may still be writing WS events to it. Clearing would orphan
      // in-flight messages — the next chunk would create a new message instead of appending.
      // Transient state is cleared only by explicit actions: clearMessages, clearSessionState,
      // or when the "done"/"error" event naturally concludes the stream.

      // Ensure the new session has a state entry
      if (!newSessionStates[sessionId]) {
        newSessionStates[sessionId] = { ...makeInitialSessionState(agent), lastAccessed: Date.now() };
      } else {
        newSessionStates[sessionId] = {
          ...newSessionStates[sessionId],
          lastAccessed: Date.now(),
        };
      }

      patches.sessionStates = newSessionStates;

      // Evict stale sessions
      const evictResult = evictStaleSessions(
        { ...state, agentStates: { ...state.agentStates, [agentId]: { ...agent, ...patches } } },
        agentId,
        sessionId,
      );



      return {
        ...evictResult,
      };
    });
  },

  /** Apply session metadata (model/provider/workspace_id) from activate_session response.
   *  Sets the session's model/provider and agent's preferredModel, plus syncs workspaceStore. */
  applySessionMeta: (
    agentId: string,
    sessionId: string,
    meta: { model?: string | null; provider?: string | null; workspace_id?: string | null },
  ) => {
    set((state) => {
      const sessionPatch: Partial<SessionChatState> = {};
      const agentPatch: Partial<AgentState> = {};
      if (typeof meta.model === "string" && meta.model) {
        sessionPatch.model = meta.model;
        agentPatch.preferredModel = meta.model;
      }
      if (typeof meta.provider === "string" && meta.provider) {
        sessionPatch.provider = meta.provider;
        agentPatch.preferredProvider = meta.provider;
      }
      if (Object.keys(sessionPatch).length === 0 && Object.keys(agentPatch).length === 0) return state;

      // Apply session and agent patches sequentially, carrying state forward
      let result = state;
      if (Object.keys(sessionPatch).length > 0) {
        const p = updateSessionState(result, agentId, sessionId, sessionPatch);
        result = { ...result, agentStates: p.agentStates };
      }
      if (Object.keys(agentPatch).length > 0) {
        const p = updateAgentState(result, agentId, agentPatch);
        result = { ...result, agentStates: p.agentStates };
      }
      return result;
    });
    // Sync workspace selection to workspaceStore
    if (typeof meta.workspace_id === "string" && meta.workspace_id) {
      useWorkspaceStore.getState().setSessionWorkspaceLocal(sessionId, meta.workspace_id as string);
    }
  },

  clearMessages: (agentId: string) => {
    const sessionId = getAgentState(get(), agentId).activeSessionId;
    if (!sessionId) return;
    set((state) => ({
      ...updateSessionState(state, agentId, sessionId, {
        messages: [],
        streamingMessageId: null,
        streamBuffer: "",
        thinkingMessageId: null,
        isInThinkPhase: false,
        isReasoning: false,
        tokenUsage: null,
        contextUsage: null,
        hasMoreMessages: false,
        messageCursor: null,
        iterationLimitPaused: null,
        pendingApproval: {},
        currentTurnId: null,
        loadError: null,
        pendingSend: false,
      }),
    }));
  },

  clearSessionState: (agentId: string, sessionId: string) => {
    set((state) => ({
      ...updateSessionState(state, agentId, sessionId, {
        messages: [],
        streamingMessageId: null,
        streamBuffer: "",
        thinkingMessageId: null,
        isInThinkPhase: false,
        tokenUsage: null,
        contextUsage: null,
        hasMoreMessages: false,
        messageCursor: null,
        iterationLimitPaused: null,
        pendingApproval: {},
        currentTurnId: null,
        loadError: null,
        isReasoning: false,
        pendingSend: false,
      }),
    }));
  },

  removeSessionState: (agentId: string, sessionId: string) => {
    set((state) => {
      const agent = getAgentState(state, agentId);
      const newSessionStates = { ...agent.sessionStates };
      delete newSessionStates[sessionId];
      return updateAgentState(state, agentId, { sessionStates: newSessionStates });
    });
  },

  connectStream: (agentId: string, gatewayUrl: string = getGatewayUrl()) => {
    resetReconnect(agentId);

    const existing = get().wsMap[agentId];
    if (existing && existing.readyState === WebSocket.OPEN) {
      console.log("[ChatStore] Reusing existing WebSocket for agent:", agentId);
      return;
    }

    if (existing) {
      existing.onopen = null;
      existing.onmessage = null;
      existing.onclose = null;
      existing.onerror = null;
      existing.close();
    }

    const wsUrl = toWsUrl(gatewayUrl, agentId);
    let ws: WebSocket;
    try {
      ws = new WebSocket(wsUrl);
    } catch (e) {
      console.warn("[ChatStore] WebSocket creation failed, will retry:", e);
      set((state) => {
        const newMap = { ...state.wsMap };
        delete newMap[agentId];
        return { wsMap: newMap };
      });
      scheduleReconnect(agentId, gatewayUrl);
      return;
    }

    ws.onopen = () => {
      console.log("[ChatStore] WebSocket connected for agent:", agentId);
      resetReconnect(agentId);
      set((state) => ({ wsMap: { ...state.wsMap, [agentId]: ws } }));

      // ADR-014: Pull repair — refresh session statuses on WS (re)connect
      useAgentStore.getState().fetchSessions(agentId);
    };

    ws.onmessage = (event) => {
      try {
        const data = JSON.parse(event.data);
        handleMessageEvent(data, set, get, agentId);
      } catch (e) {
        console.error("[ChatStore] Failed to parse WS message:", e);
      }
    };

    ws.onclose = () => {
      if (get().wsMap[agentId] !== ws) {
        console.log("[ChatStore] Stale WebSocket closed, ignoring");
        return;
      }
      console.log("[ChatStore] WebSocket closed for agent:", agentId);
      set((state) => {
        const newMap = { ...state.wsMap };
        delete newMap[agentId];
        // Clear streaming state for the active session of this agent
        const agent = getAgentState(state, agentId);
        const sessionId = agent.activeSessionId;
        const sessionPatch = sessionId
          ? updateSessionState(state, agentId, sessionId, {
            streamingMessageId: null,
            streamBuffer: "",
            thinkingMessageId: null,
            isInThinkPhase: false,
            isReasoning: false,
            pendingSend: false,
          })
          : {};
        return {
          wsMap: newMap,
          ...sessionPatch,
        };
      });
      scheduleReconnect(agentId, gatewayUrl);
    };

    ws.onerror = (err) => {
      if (get().wsMap[agentId] !== ws) {
        console.log("[ChatStore] Stale WebSocket error, ignoring");
        return;
      }
      console.warn("[ChatStore] WebSocket error:", err);
    };

    set((state) => ({
      wsMap: { ...state.wsMap, [agentId]: ws },
    }));
    // Clear active session's streaming state
    const activeSessionId = getAgentState(get(), agentId).activeSessionId;
    if (activeSessionId) {
      set((state) => ({
        ...updateSessionState(state, agentId, activeSessionId, {
          streamingMessageId: null,
          streamBuffer: "",
          thinkingMessageId: null,
          isInThinkPhase: false,
          isReasoning: false,
          tokenUsage: null,
          contextUsage: null,
        }),
      }));
    }
  },

  sendMessage: async (content: string, agentId: string, command?: string, documentIds?: string[], documents?: Array<{ id: string; filename: string; format: string; size: number; path?: string }>, imageParts?: Array<{ url: string; width: number; height: number }>) => {
    const ws = get().wsMap[agentId];
    const sessionId = getAgentState(get(), agentId).activeSessionId;

    // Add user message to the active session's state
    const userMsg: ChatMessage = {
      id: `msg-user-${Date.now()}`,
      type: "user",
      content,
      timestamp: Date.now(),
      ...getUserSenderInfo(),
    };

    // Attach document info to user message for inline rendering in the bubble
    if (documents && documents.length > 0) {
      userMsg.documents = documents.map((doc) => ({
        filename: doc.filename,
        format: doc.format,
        size: doc.size,
        documentId: doc.id,
      }));
    }

    // Attach image info to user message for inline rendering
    if (imageParts && imageParts.length > 0) {
      userMsg.imageUrls = imageParts.map((img) => img.url);
    }

    if (sessionId) {
      set((state) => ({
        ...updateSessionState(state, agentId, sessionId, {
          pendingSend: true,
          messages: [...getSessionState(state, agentId, sessionId).messages, userMsg],
          currentTurnId: null,
        }),
      }));

    }

    // Update session title immediately when first message is sent
    const activeState = getActiveSessionState(get(), agentId);
    if (sessionId) updateSessionTitleFromMessages(activeState.messages, sessionId, agentId);

    // Build multimodal content_parts when images are attached
    const contentParts = imageParts && imageParts.length > 0
      ? [
        { type: "text", text: content },
        ...imageParts.map((img) => ({
          type: "image_url",
          image_url: { url: img.url, width: img.width, height: img.height },
        })),
      ]
      : undefined;

    // Build attached context block from session state (files/selections from
    // workspace explorer right-click or editor "Add to Chat" button).
    // Passes file paths + line ranges as structured metadata in the WebSocket
    // message so the Runtime can read the actual content from the filesystem
    // and inject it into the LLM system prompt via ContextBuilder.
    // A human-readable summary is also prepended to the user message so the
    // chat history shows what was attached (LLM also sees this as fallback).
    let attachedContextBlock = "";
    let attachedContextPayload: Array<{ absPath: string; type: string; startLine?: number; endLine?: number }> | undefined;
    if (sessionId) {
      const ss = getSessionState(get(), agentId, sessionId);
      if (ss.attachedContext.length > 0) {
        const lines = ss.attachedContext.map((ctx) => {
          const lineInfo = ctx.startLine != null
            ? ` (L${ctx.startLine}${ctx.endLine && ctx.endLine !== ctx.startLine ? `-L${ctx.endLine}` : ""})`
            : "";
          return `- ${ctx.type === "directory" ? "folder: " : "file: "}\`${ctx.absPath}\`${lineInfo}`;
        });
        attachedContextBlock = `[Attached context:]\n${lines.join("\n")}\n\n`;
        attachedContextPayload = ss.attachedContext.map((ctx) => ({
          absPath: ctx.absPath,
          type: ctx.type,
          startLine: ctx.startLine,
          endLine: ctx.endLine,
        }));
      }
    }

    // Combine attached context with user message for LLM delivery.
    // visibleContent = what the user typed (stored in UI); enrichedContent = what the LLM receives.
    const enrichedContent = attachedContextBlock ? `${attachedContextBlock}${content}` : content;
    const enrichedContentParts = contentParts
      ? [{ type: "text", text: enrichedContent }, ...contentParts.filter((p) => p.type !== "text").slice(0)]
      : undefined;

    const sendViaWs = (socket: WebSocket) => {
      socket.send(JSON.stringify({
        type: "message",
        content: enrichedContent,
        command,
        ...(sessionId ? { session_id: sessionId } : {}),
        ...(documentIds && documentIds.length > 0 ? { document_ids: documentIds } : {}),
        ...(enrichedContentParts ? { content_parts: enrichedContentParts } : {}),
        ...(attachedContextPayload ? { attached_context: attachedContextPayload } : {}),
      }));

      // Clear attached context after sending (one-shot)
      if (sessionId) {
        const state = get();
        const ss = getSessionState(state, agentId, sessionId);
        if (ss.attachedContext.length > 0) {
          set((s) => updateSessionState(s, agentId, sessionId, { attachedContext: [] }));
        }
      }

      // Reset streaming state for the active session
      if (sessionId) {
        set((state) => ({
          ...updateSessionState(state, agentId, sessionId, {
            streamBuffer: "",
            streamingMessageId: null,
            thinkingMessageId: null,
            isInThinkPhase: false,
            isReasoning: false,
          }),
        }));
      }
    };

    if (ws) {
      if (ws.readyState === WebSocket.OPEN) {
        sendViaWs(ws);
        return;
      }
      if (ws.readyState === WebSocket.CONNECTING) {
        const connected = await new Promise<boolean>((resolve) => {
          const timeout = setTimeout(() => resolve(false), 2000);
          const onOpen = () => {
            clearTimeout(timeout);
            ws.removeEventListener("open", onOpen);
            ws.removeEventListener("error", onError);
            resolve(true);
          };
          const onError = () => {
            clearTimeout(timeout);
            ws.removeEventListener("open", onOpen);
            ws.removeEventListener("error", onError);
            resolve(false);
          };
          ws.addEventListener("open", onOpen);
          ws.addEventListener("error", onError);
        });
        if (connected) {
          sendViaWs(ws);
          return;
        }
      }
    }

    // Fallback: send via Tauri HTTP command
    try {
      const result = await invoke<{ message_id: string; status: string }>(
        "send_message",
        { agentId, content: enrichedContent, command, sessionId, documentIds, contentParts: enrichedContentParts, attachedContext: attachedContextPayload },
      );
      console.log("[ChatStore] Message sent via HTTP:", result);
      const replyMsg: ChatMessage = {
        id: `msg-assistant-${Date.now()}`,
        type: "system",
        content: "Message sent. Waiting for agent response... (streaming not available)",
        timestamp: Date.now(),
      };
      if (sessionId) {
        set((state) => ({
          ...updateSessionState(state, agentId, sessionId, {
            pendingSend: false,
            messages: [...getSessionState(state, agentId, sessionId).messages, replyMsg],
          }),
        }));
      }
    } catch (error) {
      console.error("[ChatStore] HTTP message send failed:", error);
      const errorMsg: ChatMessage = {
        id: `msg-error-${Date.now()}`,
        type: "system",
        content: `Failed to send message: Agent may not be connected yet. Please wait and try again.`,
        timestamp: Date.now(),
      };
      if (sessionId) {
        set((state) => ({
          ...updateSessionState(state, agentId, sessionId, {
            pendingSend: false,
            messages: [...getSessionState(state, agentId, sessionId).messages, errorMsg],
          }),
        }));
      }
    }
  },

  stopCurrentMessage: async (agentId: string) => {
    console.log("[ChatStore] Stopping current message for agent:", agentId);

    const ws = get().wsMap[agentId];
    if (ws && ws.readyState === WebSocket.OPEN) {
      const sessionId = getAgentState(get(), agentId).activeSessionId;
      ws.send(JSON.stringify({
        type: "stop",
        agentId,
        ...(sessionId ? { session_id: sessionId } : {}),
      }));
    }

    const activeSessionId = getAgentState(get(), agentId).activeSessionId;
    if (activeSessionId) {
      set((state) => ({
        ...updateSessionState(state, agentId, activeSessionId, {
          pendingSend: false,
          streamingMessageId: null,
          streamBuffer: "",
          thinkingMessageId: null,
          isInThinkPhase: false,
        }),
      }));
    }
  },

  sendStop: (agentId: string) => {
    const ws = get().wsMap[agentId];
    if (ws && ws.readyState === WebSocket.OPEN) {
      const sessionId = getAgentState(get(), agentId).activeSessionId;
      ws.send(JSON.stringify({
        type: "stop",
        agentId,
        ...(sessionId ? { session_id: sessionId } : {}),
      }));
    }
    // Optimistic: immediately mark as stopping so the UI exits "working" state
    // without waiting for the backend Stopped/SessionStateChanged event.
    const activeSessionId = getAgentState(get(), agentId).activeSessionId;
    if (activeSessionId) {
      set((state) =>
        updateSessionState(state, agentId, activeSessionId, {
          isStopping: true,
        }),
      );
    }
  },

  disconnectStream: (agentId?: string) => {
    if (agentId) {
      resetReconnect(agentId);
      const ws = get().wsMap[agentId];
      if (ws) {
        ws.onopen = null;
        ws.onmessage = null;
        ws.onclose = null;
        ws.onerror = null;
        ws.close();
      }
      set((state) => {
        const newMap = { ...state.wsMap };
        delete newMap[agentId];
        const agent = getAgentState(state, agentId);
        const sessionId = agent.activeSessionId;
        return {
          wsMap: newMap,
          ...(sessionId
            ? updateSessionState(state, agentId, sessionId, {
              streamingMessageId: null,
              streamBuffer: "",
              thinkingMessageId: null,
              isInThinkPhase: false,
              pendingSend: false,
            })
            : {}),
        };
      });
    } else {
      resetAllReconnects();
      const allWs = get().wsMap;
      for (const id of Object.keys(allWs)) {
        const ws = allWs[id];
        ws.onopen = null;
        ws.onmessage = null;
        ws.onclose = null;
        ws.onerror = null;
        ws.close();
      }
      // Clear streaming state for all agents' active sessions
      const clearedAgentStates: Record<string, AgentState> = {};
      for (const [id, agent] of Object.entries(get().agentStates)) {
        const newSessionStates = { ...agent.sessionStates };
        if (agent.activeSessionId && newSessionStates[agent.activeSessionId]) {
          newSessionStates[agent.activeSessionId] = {
            ...newSessionStates[agent.activeSessionId],
            streamingMessageId: null,
            streamBuffer: "",
            thinkingMessageId: null,
            isInThinkPhase: false,
          };
        }
        clearedAgentStates[id] = { ...agent, sessionStates: newSessionStates };
      }
      set({
        wsMap: {},
        agentStates: clearedAgentStates,
      });
    }
  },

  trimMessagesTo: (agentId: string, count: number) => {
    const sessionId = getAgentState(get(), agentId).activeSessionId;
    if (!sessionId) return;
    set((state) => {
      const session = getSessionState(state, agentId, sessionId);
      if (session.messages.length <= count) return {};
      return updateSessionState(state, agentId, sessionId, {
        messages: session.messages.slice(0, count),
        hasMoreMessages: false,
        messageCursor: null,
      });
    });
  },

  setCurrentModel: (model: string, provider: string, agentId: string) => {
    const sessionId = getAgentState(get(), agentId).activeSessionId;
    if (!sessionId) return;

    // Resolve new model's default reasoning effort from availableModels
    const models = get().availableModels;
    const newModelEntry = models.find((m) => m.name === model && m.provider === provider);
    const defaultEffort = newModelEntry?.default_reasoning_effort ?? null;

    // Update session model + reset reasoningEffort to new model's default
    set((state) => updateSessionState(state, agentId, sessionId, {
      model,
      provider,
      reasoningEffort: defaultEffort,
    }));
    // Update agent's default model (new sessions inherit this)
    set((state) => updateAgentState(state, agentId, { preferredModel: model, preferredProvider: provider }));

    const ws = get().wsMap[agentId];
    if (ws?.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify({ type: "model_switch", model, provider, agentId, session_id: sessionId }));
    }
  },
  setReasoningEffort: (effort: string, agentId: string) => {
    const sessionId = getAgentState(get(), agentId).activeSessionId;
    if (!sessionId) return;

    // Optimistically update frontend state (Runtime will confirm)
    set((state) => updateSessionState(state, agentId, sessionId, { reasoningEffort: effort }));

    const ws = get().wsMap[agentId];
    if (ws?.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify({ type: "reasoning_effort", effort, agentId, session_id: sessionId }));
    }
  },
  setAvailableModels: (models: ModelEntry[]) => {
    set({ availableModels: models });
  },
  continueExecution: async (agentId: string) => {
    try {
      const sessionId = getAgentState(get(), agentId).activeSessionId;
      const resp = await fetch(`${getGatewayUrl()}/api/agents/${agentId}/continue`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ ...(sessionId ? { session_id: sessionId } : {}) }),
      });
      if (resp.ok) {
        if (sessionId) {
          set((state) => ({
            ...updateSessionState(state, agentId, sessionId, { pendingSend: true, iterationLimitPaused: null }),
          }));
        }
      }
    } catch (error) {
      console.error("[ChatStore] Failed to send continue signal:", error);
    }
  },
  resolveApproval: (agentId: string) => {
    const sessionId = getAgentState(get(), agentId).activeSessionId;
    if (!sessionId) return;
    set((state) => updateSessionState(state, agentId, sessionId, { pendingApproval: {} }));
  },
  resolveApprovalByToolCallId: (agentId: string, toolCallId: string) => {
    const sessionId = getAgentState(get(), agentId).activeSessionId;
    if (!sessionId) return;
    set((state) => {
      const prevPending = getSessionState(state, agentId, sessionId).pendingApproval;
      const nextPending = { ...prevPending };
      delete nextPending[toolCallId];
      return updateSessionState(state, agentId, sessionId, { pendingApproval: nextPending });
    });
  },
  resolveQuestion: (agentId: string) => {
    const sessionId = getAgentState(get(), agentId).activeSessionId;
    if (!sessionId) return;
    set((state) => updateSessionState(state, agentId, sessionId, { pendingQuestion: null }));
  },
  loadConversationHistory: async (agentId: string) => {
    try {
      const resp = await fetch(`${getGatewayUrl()}/api/agents/${agentId}/conversations/latest`);
      if (!resp.ok) return;
      const data = await resp.json() as { session_id?: string; messages?: Array<{ role: string; content: string; timestamp: number; turn_index: number }> };

      if (!data.messages || data.messages.length === 0) return;

      const historyMessages: ChatMessage[] = data.messages.map((msg) => ({
        id: `history-${msg.turn_index}-${msg.role}-${msg.timestamp}`,
        type: (msg.role === "user"
          ? "user"
          : msg.role === "assistant"
            ? "assistant"
            : msg.role === "think" || msg.role === "thought"
              ? "thought"
              : "system") as ChatMessage["type"],
        content: msg.content,
        timestamp: msg.timestamp * 1000,
      }));

      // Use session_id from response if available, else fall back to active session
      const sessionId = data.session_id ?? getAgentState(get(), agentId).activeSessionId;
      if (sessionId) {
        set((state) => updateSessionState(state, agentId, sessionId, { messages: historyMessages }));
      }
    } catch (e) {
      console.error("[ChatStore] Failed to load conversation history:", e);
      const sessionId = getAgentState(get(), agentId).activeSessionId;
      if (sessionId) {
        set((state) => updateSessionState(state, agentId, sessionId, { messages: [] }));
      }
    }
  },

  loadSessionMessages: async (
    agentId: string,
    sessionId: string,
    cursor?: string,
    limit: number = 50,
    direction: string = "backward",
  ) => {
    // Skip loading if this session is currently streaming or pending send
    const sessionState = getSessionState(get(), agentId, sessionId);
    if (sessionState.streamingMessageId != null || isSessionSending(sessionState)) {
      console.log(`[ChatStore] Skipping loadSessionMessages — session ${sessionId} is active (streaming=${sessionState.streamingMessageId != null}, pendingSend=${sessionState.pendingSend}, status=${sessionState.sessionStatus?.status})`);
      return;
    }

    const seq = get().loadSequence + 1;
    set({ loadSequence: seq });

    const oldController = get().abortController;
    if (oldController) {
      oldController.abort();
    }
    const controller = new AbortController();
    set({ abortController: controller });

    if (!cursor) {
      set((state) => ({
        ...updateSessionState(state, agentId, sessionId, { isLoadingSession: true, loadError: null }),
      }));
    }

    try {
      const params = new URLSearchParams();
      params.set("limit", String(limit));
      params.set("direction", direction);
      if (cursor) params.set("cursor", cursor);

      const resp = await fetch(
        `${getGatewayUrl()}/api/agents/${agentId}/sessions/${sessionId}/messages?${params}`,
        { signal: controller.signal },
      );

      if (get().loadSequence !== seq) {
        console.log(`[ChatStore] Discarding stale loadSessionMessages response (seq ${seq} vs current ${get().loadSequence})`);
        return;
      }

      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);

      const data = (await resp.json()) as PaginatedMessages;

      if (get().loadSequence !== seq) {
        console.log(`[ChatStore] Discarding stale response after json parse (seq ${seq} vs current ${get().loadSequence})`);
        return;
      }

      console.log(`[ChatStore] Loaded ${data.messages?.length ?? 0} messages for session ${sessionId}`);

      const converted = mergeDocumentUploads(data.messages ?? [], agentId);

      set((state) => {
        if (state.loadSequence !== seq) {
          console.log(`[ChatStore] Discarding state update — sequence changed`);
          return {};
        }

        if (cursor) {
          const existingIds = new Set(getSessionState(state, agentId, sessionId).messages.map((m) => m.id));
          const newMessages = converted.filter((m) => !existingIds.has(m.id));
          return {
            ...updateSessionState(state, agentId, sessionId, {
              messages: [...newMessages, ...getSessionState(state, agentId, sessionId).messages],
              hasMoreMessages: data.has_more,
              messageCursor: data.cursor,
              isLoadingSession: false,
              loadError: null,
            }),
            isLoadingMore: false,
          };
        }

        return {
          ...updateSessionState(state, agentId, sessionId, {
            messages: converted,
            hasMoreMessages: data.has_more,
            messageCursor: data.cursor,
            isLoadingSession: false,
            loadError: null,
          }),
          isLoadingMore: false,
        };
      });
    } catch (e: unknown) {
      if (get().loadSequence !== seq) {
        console.log(`[ChatStore] Discarding stale error response (seq ${seq})`);
        return;
      }
      if (e instanceof DOMException && e.name === "AbortError") {
        console.log(`[ChatStore] loadSessionMessages aborted (seq ${seq})`);
        set((state) => ({
          ...updateSessionState(state, agentId, sessionId, { isLoadingSession: false }),
          isLoadingMore: false,
        }));
        return;
      }
      console.error("[ChatStore] Failed to load session messages:", e);
      set((state) => ({
        ...updateSessionState(state, agentId, sessionId, {
          messages: [],
          hasMoreMessages: false,
          messageCursor: null,
          isLoadingSession: false,
          loadError: `${i18n.t("chatPanel.sessionLoadFailed")}: ${e instanceof Error ? e.message : String(e)}`,
        }),
        isLoadingMore: false,
      }));
    } finally {
      const currentController = get().abortController;
      if (currentController === controller) {
        set({ abortController: null });
      }
    }
  },

  abortSessionLoad: () => {
    const controller = get().abortController;
    if (controller) {
      controller.abort();
      set({ abortController: null });
    }
    set((state) => ({ loadSequence: state.loadSequence + 1 }));
  },

  loadMoreMessages: async (agentId: string, sessionId: string) => {
    const { isLoadingMore } = get();
    const sessionState = getSessionState(get(), agentId, sessionId);
    if (isLoadingMore || !sessionState.hasMoreMessages || !sessionState.messageCursor) return;
    set({ isLoadingMore: true });
    try {
      await get().loadSessionMessages(agentId, sessionId, sessionState.messageCursor, 50, "backward");
    } finally {
      set({ isLoadingMore: false });
    }
  },

  toggleTreeExpandedPath: (agentId: string, sessionId: string, relPath: string) => {
    set((state) => {
      const ss = getSessionState(state, agentId, sessionId);
      const current = ss.treeExpandedPaths;
      const idx = current.indexOf(relPath);
      const next = idx >= 0
        ? current.filter((p) => p !== relPath)
        : [...current, relPath];
      return updateSessionState(state, agentId, sessionId, { treeExpandedPaths: next });
    });
  },

  addAttachedContext: (agentId: string, sessionId: string, item: { id: string; type: "file" | "directory" | "selection"; name: string; absPath: string; startLine?: number; endLine?: number }) => {
    set((state) => {
      const ss = getSessionState(state, agentId, sessionId);
      // Avoid duplicates
      if (ss.attachedContext.some((c) => c.id === item.id)) return {};
      return updateSessionState(state, agentId, sessionId, {
        attachedContext: [...ss.attachedContext, item],
      });
    });
  },

  removeAttachedContext: (agentId: string, sessionId: string, id: string) => {
    set((state) => {
      const ss = getSessionState(state, agentId, sessionId);
      return updateSessionState(state, agentId, sessionId, {
        attachedContext: ss.attachedContext.filter((c) => c.id !== id),
      });
    });
  },

  clearAttachedContext: (agentId: string, sessionId: string) => {
    set((state) => updateSessionState(state, agentId, sessionId, { attachedContext: [] }));
  },

  // ADR-015 Phase 5: Pull initial session state from backend.
  // Maps the /api/agents/{id}/sessions/{sid}/state response to SessionChatState fields.
  // Errors are non-fatal — warns and returns without blocking startup.
  fetchSessionState: async (agentId: string, sessionId: string) => {
    try {
      const resp = await fetch(
        `${getGatewayUrl()}/api/agents/${agentId}/sessions/${sessionId}/state`,
      );
      if (!resp.ok) {
        console.warn(`[ChatStore] fetchSessionState HTTP ${resp.status} for session ${sessionId}`);
        return;
      }
      const data = await resp.json() as {
        session_id: string;
        status?: string;
        model?: string | null;
        provider?: string | null;
        workspace_id?: string | null;
        ratio?: number | null;
        reasoning_effort?: string | null;
        temperature?: number | null;
      };
      const sessionPatch: Partial<SessionChatState> = {};
      if (typeof data.model === "string" && data.model) sessionPatch.model = data.model;
      if (typeof data.provider === "string" && data.provider) sessionPatch.provider = data.provider;
      if (typeof data.ratio === "number") sessionPatch.ratio = data.ratio;
      if (typeof data.reasoning_effort === "string" && data.reasoning_effort) sessionPatch.reasoningEffort = data.reasoning_effort;
      if (typeof data.temperature === "number") sessionPatch.temperature = data.temperature;
      if (Object.keys(sessionPatch).length > 0) {
        set((state) => updateSessionState(state, agentId, sessionId, sessionPatch));
      }
      // Sync workspace to workspaceStore if present
      if (typeof data.workspace_id === "string" && data.workspace_id) {
        useWorkspaceStore.getState().setSessionWorkspaceLocal(sessionId, data.workspace_id);
      }
    } catch (e) {
      console.warn("[ChatStore] fetchSessionState failed:", e);
    }
  },
}));

// ── Session title persistence ─────────────────────────────────────────

function makeSessionTitle(content: string): string {
  return content.replace(/\n/g, " ").trim().substring(0, 30);
}

function updateSessionTitleFromMessages(messages: ChatMessage[], sessionId: string, agentId?: string) {
  const firstUserMsg = messages.find((m) => m.type === "user");
  if (!firstUserMsg || !firstUserMsg.content) return;

  // Don't overwrite an existing title — historical sessions should keep
  // their original title. Only set title for brand-new sessions.
  if (agentId) {
    const sessions = useAgentStore.getState().agents[agentId]?.sessions ?? [];
    const existingSession = sessions.find(
      (s) => s.session_id === sessionId,
    );
    if (existingSession?.title && existingSession.title.trim() !== "") return;
  } else {
    // agentId unknown — search all agents for the session
    const allAgents = useAgentStore.getState().agents;
    for (const storage of Object.values(allAgents)) {
      const existing = storage.sessions.find((s) => s.session_id === sessionId);
      if (existing?.title && existing.title.trim() !== "") return;
    }
  }

  const title = makeSessionTitle(firstUserMsg.content);

  useAgentStore.getState().updateSessionTitle(sessionId, title);

  const cacheKey = `${sessionId}::${title}`;
  const persistedTitles = useChatStore.getState().persistedTitles;
  if (persistedTitles.has(cacheKey)) return;

  // Add to persisted set
  const newSet = new Set(persistedTitles);
  newSet.add(cacheKey);
  useChatStore.setState({ persistedTitles: newSet });

  if (agentId) {
    fetch(`${getGatewayUrl()}/api/agents/${agentId}/sessions/${sessionId}/title`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ title }),
    }).catch((e) => {
      console.warn("[ChatStore] Failed to persist title to backend:", e);
    });
  }
}

// ── Conversation entry conversion ─────────────────────────────────────

/** Strip leading/trailing `<summary>...</summary>` tags from a compaction
 *  summary string (the LLM is instructed to wrap output in those tags;
 *  we don't need them in the UI). Returns the inner text trimmed. If the
 *  tags aren't present, returns the original input trimmed. */
function stripSummaryTags(text: string): string {
  const trimmed = text.trim();
  const match = trimmed.match(/^<summary>([\s\S]*?)<\/summary>$/i);
  return (match ? match[1] : trimmed).trim();
}

function convertConversationEntry(entry: ConversationEntry, agentId: string): ChatMessage {
  // Compaction events: rendered as a folded summary card. Mirrors the
  // backend `kind="compaction"` JSONL marker. Detected BEFORE role-based
  // mapping because the underlying role is "system" but we render it
  // distinctly.
  if (entry.kind === "compaction") {
    const meta = (entry.metadata ?? {}) as Record<string, unknown>;
    const agentInfo = getAgentSenderInfo(agentId);
    return {
      id: entry.id,
      type: "compaction",
      content: stripSummaryTags(entry.content),
      timestamp: new Date(entry.ts).getTime(),
      senderDisplayName: agentInfo.senderDisplayName,
      senderRole: agentInfo.senderRole,
      compactionMeta: {
        compacted_from_id: meta.compacted_from_id as string | undefined,
        compacted_to_id: meta.compacted_to_id as string | undefined,
        keep_last_rounds: (meta.keep_last_rounds as number) ?? 0,
        model: meta.model as string | undefined,
        before_tokens: (meta.before_tokens as number) ?? 0,
        after_tokens: (meta.after_tokens as number) ?? 0,
      },
    };
  }

  const base: ChatMessage = {
    id: entry.id,
    type: (entry.role === "think" ? "thought" : entry.role) as ChatMessage["type"],
    content: entry.content,
    timestamp: new Date(entry.ts).getTime(),
  };

  if (entry.role === "user") {
    const userInfo = getUserSenderInfo();
    base.senderDisplayName = userInfo.senderDisplayName;
  } else if (entry.role === "assistant" || entry.role === "think" || entry.role === "thought" || entry.role === "tool_call" || entry.role === "tool_result") {
    const agentInfo = getAgentSenderInfo(agentId);
    base.senderDisplayName = agentInfo.senderDisplayName;
    base.senderRole = agentInfo.senderRole;
  }

  const meta = entry.metadata;
  if (!meta) return base;

  // document_upload entries: extract fields from metadata
  if (meta.type === "document_upload") {
    base.type = "document_upload";
    base.documentId = meta.document_id as string | undefined;
    base.documentFormat = meta.format as string | undefined;
    base.documentSize = meta.size_bytes as number | undefined;
    base.documentPath = meta.path as string | undefined;
    return base;
  }

  if (entry.role === "tool_call" || entry.role === "tool_result") {
    base.toolName = meta.tool_name as string | undefined;
    base.toolData = meta as Record<string, unknown>;
    if (entry.role === "tool_result") {
      base.toolStatus = meta.success === false ? "error" : "success";
    }
  }

  if (entry.role === "think" || entry.role === "thought") {
    base.startTime = (meta.startTime as number) ?? undefined;
    base.endTime = (meta.endTime as number) ?? undefined;
  }

  return base;
}

/**
 * Merge document_upload entries into their following user messages,
 * and strip document-enriched content (appended by backend doc_reader)
 * from user message content.
 *
 * Backend persists document uploads as separate system-role entries with
 * metadata.type === "document_upload", and appends document parsed text to
 * the user message content. This reverses both to match the frontend's
 * optimistic message format (documents array inline in user message).
 */
function mergeDocumentUploads(entries: ConversationEntry[], agentId: string): ChatMessage[] {
  const ENRICHMENT_TEXT = "The following documents were uploaded by the user.";
  const result: ChatMessage[] = [];
  let pendingDocs: ChatMessage["documents"] = [];

  for (const entry of entries) {
    // Collect document_upload entries to merge into the following user message
    if (entry.metadata?.type === "document_upload") {
      const meta = entry.metadata;
      pendingDocs.push({
        filename: (meta.filename as string) || "",
        format: (meta.format as string) || "unknown",
        size: meta.size_bytes as number | undefined,
        documentId: meta.document_id as string | undefined,
      });
      continue;
    }

    const msg = convertConversationEntry(entry, agentId);

    // Attach pending document info to the next user message
    if (msg.type === "user" && pendingDocs.length > 0) {
      msg.documents = pendingDocs;
      pendingDocs = [];

      // Strip enriched document content from user message content
      if (msg.content) {
        const idx = msg.content.indexOf(ENRICHMENT_TEXT);
        if (idx !== -1) {
          // Strip from the enrichment text start, handling optional "\n\n" prefix
          msg.content = msg.content.substring(0, idx).replace(/\n\n$/, "");
        }
      }
    }

    result.push(msg);
  }

  return result;
}

// ── WebSocket event handler — routes by event.session_id ──────────────

const CONTENT_EVENT_TYPES = new Set([
  "reasoning_started", "chunk", "tool_call", "tool_result",
  "done", "error", "tool_approval_needed", "ask_question", "iteration_limit_paused",
  "context_usage", "session_state_changed", "stopped", "todo_list_updated",
  "compacting_started", "compacting_ended", "model_confirmed", "reasoning_effort_confirmed",
]);

function handleMessageEvent(
  data: Record<string, unknown>,
  set: (fn: Partial<ChatStore> | ((state: ChatStore) => Partial<ChatStore>)) => void,
  get: () => ChatStore,
  agentId: string,
) {
  const eventType = data.type as string;

  // ── DIAG: log every incoming WS message ──
  // if (eventType === "tool_approval_needed" || eventType === "tool_call") {
  //   console.log("[DIAG:handleMessageEvent]", eventType, JSON.stringify(data));
  // }

  // For content events: route to the session specified by event.session_id
  // If no session_id in event, fall back to the agent's active session.
  // This is the core fix: events go directly to their owning session,
  // NOT filtered by currentSessionId. Background sessions receive their
  // events correctly; non-active sessions just don't get rendered.
  let sid: string | null = null;

  if (CONTENT_EVENT_TYPES.has(eventType)) {
    const eventSessionId = data.session_id as string | undefined;
    if (eventSessionId != null) {
      sid = eventSessionId;
    } else {
      // Backward compat: no session_id → use active session
      sid = getAgentState(get(), agentId).activeSessionId;
    }
    if (!sid) return;

    // Ensure the session state entry exists
    const agent = getAgentState(get(), agentId);
    if (!agent.sessionStates[sid]) {
      set((state) => ({
        ...updateSessionState(state, agentId, sid!, { lastAccessed: Date.now() }),
      }));
    }
  }

  switch (eventType) {
    case "connected":
      break;

    case "ack":
      break;

    case "stop_received":
      // Gateway acknowledges that the stop request was received and
      // forwarded to the Runtime.  This is NOT a state transition —
      // the Runtime may still be streaming.  The real "stopped" event
      // arrives later via the bridge channel after the Runtime actually
      // processes the interrupt.
      break;

    case "reasoning_started":
      if (sid) {
        set((state) => updateSessionState(state, agentId, sid, { isReasoning: true, isCompacting: false }));
      }
      break;

    case "chunk": {
      if (!sid) break;
      const delta = (data.delta ?? data.content) as string;
      const reasoningDelta = data.reasoning_content as string | undefined;

      set((state) => {
        const ss = getSessionState(state, agentId, sid!);

        if (reasoningDelta) {
          if (ss.thinkingMessageId) {
            return updateSessionState(state, agentId, sid!, {
              messages: ss.messages.map((msg) =>
                msg.id === ss.thinkingMessageId
                  ? { ...msg, content: msg.content + reasoningDelta }
                  : msg,
              ),
              isReasoning: false,
            });
          } else {
            const thinkMsgId = `msg-think-${Date.now()}`;
            const thinkMsg: ChatMessage = {
              id: thinkMsgId,
              type: "thought",
              content: reasoningDelta,
              timestamp: Date.now(),
              startTime: Date.now(),
              ...getAgentSenderInfo(agentId),
            };
            return updateSessionState(state, agentId, sid!, {
              messages: [...ss.messages, thinkMsg],
              thinkingMessageId: thinkMsgId,
              isReasoning: false,
            });
          }
        }

        let messages = ss.messages;
        if (ss.thinkingMessageId && delta) {
          const now = Date.now();
          messages = messages.map((msg) =>
            msg.id === ss.thinkingMessageId
              ? { ...msg, endTime: now }
              : msg,
          );
        }

        const newBuffer = ss.streamBuffer + delta;

        if (ss.isInThinkPhase && ss.thinkingMessageId) {
          const closeIdx = newBuffer.indexOf("</thinking>");
          if (closeIdx >= 0) {
            const thinkStart = newBuffer.indexOf("<thinking>");
            const thinkContent = newBuffer.substring(thinkStart + 10, closeIdx);
            const replyContent = newBuffer.substring(closeIdx + 11).trimStart();

            const now = Date.now();
            const finalMessages = messages.map((msg) =>
              msg.id === ss.thinkingMessageId
                ? { ...msg, content: thinkContent, endTime: now }
                : msg,
            );

            const assistantMsgId = `msg-assistant-${Date.now()}`;
            const assistantMsg: ChatMessage = {
              id: assistantMsgId,
              type: "assistant",
              content: replyContent,
              timestamp: Date.now(),
              ...getAgentSenderInfo(agentId),
            };

            return updateSessionState(state, agentId, sid!, {
              messages: [...finalMessages, assistantMsg],
              streamBuffer: "",
              isInThinkPhase: false,
              thinkingMessageId: null,
              streamingMessageId: assistantMsgId,
              isReasoning: false,
            });
          } else {
            const thinkStart = newBuffer.indexOf("<thinking>");
            const thinkContent = newBuffer.substring(thinkStart + 10);
            return updateSessionState(state, agentId, sid!, {
              messages: ss.messages.map((msg) =>
                msg.id === ss.thinkingMessageId
                  ? { ...msg, content: thinkContent }
                  : msg,
              ),
              streamBuffer: newBuffer,
              isReasoning: false,
            });
          }
        }

        const trimmed = newBuffer.trimStart();
        const THINK_OPEN = "<thinking>";

        if (trimmed.startsWith(THINK_OPEN)) {
          const thinkStart = newBuffer.indexOf("<thinking>");
          const thinkMsgId = `msg-think-${Date.now()}`;
          const thinkMsg: ChatMessage = {
            id: thinkMsgId,
            type: "thought",
            content: newBuffer.substring(thinkStart + 10),
            timestamp: Date.now(),
            startTime: Date.now(),
            ...getAgentSenderInfo(agentId),
          };
          return updateSessionState(state, agentId, sid!, {
            messages: [...ss.messages, thinkMsg],
            streamBuffer: newBuffer,
            isInThinkPhase: true,
            thinkingMessageId: thinkMsgId,
            streamingMessageId: thinkMsgId,
            isReasoning: false,
          });
        }

        if (trimmed.length > 0) {
          const definitelyNotThink =
            trimmed[0] !== "<" ||
            (trimmed.length >= THINK_OPEN.length && !trimmed.startsWith(THINK_OPEN));

          if (definitelyNotThink) {
            if (ss.streamingMessageId && !ss.isInThinkPhase) {
              return updateSessionState(state, agentId, sid!, {
                messages: messages.map((msg) =>
                  msg.id === ss.streamingMessageId
                    ? { ...msg, content: msg.content + delta }
                    : msg,
                ),
                streamBuffer: newBuffer,
                thinkingMessageId: null,
                isReasoning: false,
              });
            } else {
              const assistantMsgId = `msg-assistant-${Date.now()}`;
              const assistantMsg: ChatMessage = {
                id: assistantMsgId,
                type: "assistant",
                content: newBuffer,
                timestamp: Date.now(),
              };
              return updateSessionState(state, agentId, sid!, {
                messages: [...messages, assistantMsg],
                streamBuffer: newBuffer,
                streamingMessageId: assistantMsgId,
                thinkingMessageId: null,
                isReasoning: false,
              });
            }
          }
        }

        return updateSessionState(state, agentId, sid!, { streamBuffer: newBuffer, isReasoning: false });
      });
      break;
    }

    case "tool_call": {
      if (!sid) break;
      const toolName = data.name as string;
      const toolCallId = (data.tool_call_id ?? data.id) as string | undefined;
      const params = data.params as Record<string, unknown>;

      const currentState = get();
      const ss = getSessionState(currentState, agentId, sid);
      let turnId = ss.currentTurnId;
      if (!turnId) {
        turnId = `turn-${Date.now()}`;
      }

      const toolMsg: ChatMessage = {
        id: `msg-tool-${Date.now()}-${Math.random().toString(36).slice(2, 7)}`,
        type: "tool_call",
        content: JSON.stringify(params, null, 2),
        timestamp: Date.now(),
        toolName,
        toolCallId,
        toolData: params,
        turnId,
        startTime: Date.now(),
      };

      set((state) => {
        const ss = getSessionState(state, agentId, sid!);
        let messages = ss.messages;
        if (ss.thinkingMessageId) {
          const now = Date.now();
          messages = messages.map((msg) =>
            msg.id === ss.thinkingMessageId
              ? { ...msg, endTime: now }
              : msg,
          );
        }
        return {
          ...updateSessionState(state, agentId, sid!, {
            messages: [...messages, toolMsg],
            streamingMessageId: null,
            thinkingMessageId: null,
            isInThinkPhase: false,
            streamBuffer: "",
            currentTurnId: turnId,
            isReasoning: false,
          }),
        };
      });
      break;
    }

    case "tool_result": {
      if (!sid) break;
      const toolName = data.name as string;
      const result = data.result as Record<string, unknown>;
      const currentState = get();
      const ss = getSessionState(currentState, agentId, sid);

      const resultMsg: ChatMessage = {
        id: `msg-result-${Date.now()}-${Math.random().toString(36).slice(2, 7)}`,
        type: "tool_result",
        content: JSON.stringify(result, null, 2),
        timestamp: Date.now(),
        toolName,
        toolData: result,
        toolStatus: "success",
        turnId: ss.currentTurnId || undefined,
      };
      set((state) => updateSessionState(state, agentId, sid!, {
        messages: [...getSessionState(state, agentId, sid!).messages, resultMsg],
      }));
      break;
    }

    case "done": {
      if (!sid) break;
      const usage = data.usage as TokenUsage | undefined;
      const content = data.content as string | undefined;
      const reasoningContent = data.reasoning_content as string | undefined;
      set((state) => {
        const ss = getSessionState(state, agentId, sid!);
        let messages = [...ss.messages];

        if (content) {
          if (ss.streamingMessageId) {
            // Streaming message exists — chunk events have been filling it.
            // Only fill content if it's still empty (e.g. no chunk arrived yet).
            const idx = messages.findIndex((m) => m.id === ss.streamingMessageId);
            if (idx >= 0 && !messages[idx].content) {
              messages[idx] = { ...messages[idx], content };
            }
          } else {
            // streamingMessageId was cleared (e.g. by session_state_changed racing
            // ahead of done, or by sendMessage reset). Avoid creating a DUPLICATE
            // assistant message — look for an existing assistant message at the end
            // that might be the chunk-filled one with the same content.
            const lastMsg = messages[messages.length - 1];
            if (lastMsg?.type === "assistant" && lastMsg?.content === content) {
              // Chunk already created this message — just mark it as finalized
              messages[messages.length - 1] = { ...lastMsg, endTime: Date.now() };
            } else if (lastMsg?.type === "assistant" && !lastMsg?.endTime) {
              // Last assistant message exists but content differs — update it
              if (!lastMsg.content) {
                messages[messages.length - 1] = { ...lastMsg, content };
              }
            } else {
              // No existing assistant message at all — create one
              const assistantMsgId = `msg-assistant-${Date.now()}`;
              const assistantMsg: ChatMessage = {
                id: assistantMsgId,
                type: "assistant",
                content,
                timestamp: Date.now(),
              };
              messages = [...messages, assistantMsg];
            }
          }
        }

        if (reasoningContent && !ss.thinkingMessageId) {
          const thinkMsgId = `msg-think-${Date.now()}`;
          const now = Date.now();
          const thinkMsg: ChatMessage = {
            id: thinkMsgId,
            type: "thought",
            content: reasoningContent,
            timestamp: now,
            startTime: now,
            endTime: now,
          };
          messages = [...messages, thinkMsg];
        }

        if (ss.thinkingMessageId) {
          const endTime = Date.now();
          messages = messages.map((msg) =>
            msg.id === ss.thinkingMessageId && !msg.endTime
              ? { ...msg, endTime }
              : msg,
          );
        }

        return {
          ...updateSessionState(state, agentId, sid!, {
            messages,
            streamingMessageId: null,
            tokenUsage: usage ?? ss.tokenUsage,
            currentTurnId: null,
            streamBuffer: "",
            thinkingMessageId: null,
            isInThinkPhase: false,
            isReasoning: false,
            isCompacting: false,
            pendingSend: false,
            isStopping: false,
          }),
        };
      });
      const doneSessionState = getSessionState(get(), agentId, sid);
      updateSessionTitleFromMessages(doneSessionState.messages, sid, agentId);
      break;
    }

    case "model_confirmed": {
      const confirmedModel = data.model as string;
      const confirmedProvider = data.provider as string | undefined;
      console.log("[ChatStore] Model switch confirmed:", confirmedModel, confirmedProvider);
      if (confirmedModel && sid) {
        // Resolve new model's default reasoning effort
        const models = get().availableModels;
        const newModelEntry = models.find((m) => m.name === confirmedModel && m.provider === (confirmedProvider ?? ""));
        const defaultEffort = newModelEntry?.default_reasoning_effort ?? null;

        // Update session model (current session only)
        set((state) => updateSessionState(state, agentId, sid!, {
          model: confirmedModel,
          provider: confirmedProvider ?? "",
          reasoningEffort: defaultEffort,
        }));
        // Update agent's default model (new sessions inherit this)
        set((state) => updateAgentState(state, agentId, {
          preferredModel: confirmedModel,
          preferredProvider: confirmedProvider ?? null,
        }));
      }
      break;
    }

    case "reasoning_effort_confirmed": {
      const confirmedEffort = data.effort as string;
      console.log("[ChatStore] Reasoning effort confirmed:", confirmedEffort);
      if (confirmedEffort && sid) {
        set((state) => updateSessionState(state, agentId, sid!, {
          reasoningEffort: confirmedEffort,
        }));
      }
      break;
    }

    case "error": {
      if (!sid) break;
      // Backend sends user_message as content, plus detail and error_type
      const errorMsg = (data.content ?? data.message) as string;
      const errorDetail = (data.detail) as string | undefined;
      const errorType = (data.error_type) as string | undefined;
      console.error("[ChatStore] Server error:", errorMsg, errorDetail);
      const errMsg: ChatMessage = {
        id: `msg-error-${Date.now()}`,
        type: "error",
        content: errorMsg as string,
        errorDetail: errorDetail || undefined,
        errorType: errorType || undefined,
        timestamp: Date.now(),
        ...getAgentSenderInfo(agentId),
      };
      set((state) => ({
        ...updateSessionState(state, agentId, sid!, {
          messages: [...getSessionState(state, agentId, sid!).messages, errMsg],
          streamingMessageId: null,
          streamBuffer: "",
          thinkingMessageId: null,
          isInThinkPhase: false,
          isReasoning: false,
          isCompacting: false,
          pendingSend: false,
          isStopping: false,
        }),
      }));
      break;
    }

    case "stopped": {
      if (!sid) break;
      set((state) => {
        const ss = getSessionState(state, agentId, sid!);
        let messages = [...ss.messages];
        // Finalize streaming message (Stopped is only emitted mid-streaming)
        if (ss.streamingMessageId) {
          messages = messages.map((msg) =>
            msg.id === ss.streamingMessageId
              ? { ...msg, endTime: Date.now() }
              : msg,
          );
        }
        return {
          ...updateSessionState(state, agentId, sid!, {
            messages,
            streamingMessageId: null,
            streamBuffer: "",
            thinkingMessageId: null,
            isInThinkPhase: false,
            isReasoning: false,
            isCompacting: false,
            pendingSend: false,
            isStopping: false,
          }),
        };
      });
      break;
    }

    case "tool_approval_needed": {
      console.log("[DIAG:tool_approval_needed]", {
        sid,
        agentId,
        "data.tool_call_id": data.tool_call_id,
        "data.request_id": data.request_id,
        "data.session_id": data.session_id,
        "activeSessionId": getAgentState(get(), agentId).activeSessionId,
      });
      if (sid) {
        const approvalEvent = data as unknown as ToolApprovalNeededEvent;
        set((state) => {
          const agentState = state.agentStates[agentId];
          const prevPending = agentState?.sessionStates[sid]?.pendingApproval || {};
          const key = approvalEvent.tool_call_id || approvalEvent.request_id;
          const newPending = { ...prevPending, [key]: approvalEvent };
          console.log("[DIAG:tool_approval_needed:set]", {
            sid,
            key,
            prevKeys: Object.keys(prevPending),
            newKeys: Object.keys(newPending),
            approvalKeys: Object.keys(agentState?.sessionStates[sid]?.pendingApproval || {}),
          });
          return updateSessionState(state, agentId, sid, {
            pendingApproval: newPending,
          });
        });
      } else {
        console.warn("[DIAG:tool_approval_needed] DROPPED — sid is null!");
      }
      break;
    }

    case "ask_question":
      if (sid) {
        set((state) => updateSessionState(state, agentId, sid, {
          pendingQuestion: data as unknown as AskQuestionEvent,
        }));
      }
      break;

    case "memory_updated":
      console.log("[WS] Memory updated event:", data);
      break;

    case "skill_executed":
      console.log("[WS] Skill executed event:", data);
      break;

    case "compacting_started":
      if (sid) {
        set((state) => updateSessionState(state, agentId, sid, { isCompacting: true }));
      }
      break;

    case "compacting_ended":
      if (sid) {
        set((state) => updateSessionState(state, agentId, sid, { isCompacting: false }));
      }
      break;

    case "context_usage": {
      if (sid) {
        const usage = data as unknown as ContextUsageInfo;
        console.log("[ChatStore] context_usage RECEIVED for agent:", agentId, usage);
        set((state) => updateSessionState(state, agentId, sid, { contextUsage: usage, isCompacting: false }));
      }
      break;
    }

    case "iteration_limit_paused": {
      if (sid) {
        const { iteration, max_iterations, message } = data as {
          iteration: number;
          max_iterations: number;
          message: string;
        };
        set((state) => updateSessionState(state, agentId, sid, {
          iterationLimitPaused: {
            iteration,
            maxIterations: max_iterations,
            message,
          },
        }));
      }
      break;
    }

    // ADR-014: Session lifecycle status changed — source of truth from backend
    case "session_state_changed": {
      if (sid) {
        const status = data.status as SessionStatus | undefined;
        if (status) {
          set((state) => {
            const sessionPatch: Partial<SessionChatState> = { sessionStatus: status, pendingSend: false };

            // ADR-012: Backend includes per-session model/provider (from JSONL metadata).
            if (typeof data.model === "string") sessionPatch.model = data.model as string;
            if (typeof data.provider === "string") sessionPatch.provider = data.provider as string;
            // Model chars/token ratio from API calibration (for status panel display).
            if (typeof data.ratio === "number") sessionPatch.ratio = data.ratio as number;
            // Reasoning effort level (thinking level) from Runtime session state.
            if (typeof data.reasoning_effort === "string") sessionPatch.reasoningEffort = data.reasoning_effort as string;
            // Temperature override from Runtime session state.
            if (typeof data.temperature === "number") sessionPatch.temperature = data.temperature as number;

            // When status transitions FROM Streaming, clear transient streaming state.
            // IMPORTANT: Before clearing thinkingMessageId, stamp endTime on the thinking
            // message so the ThinkBlock timer stops. Without this, the done event (which
            // arrives AFTER session_state_changed) cannot find the message to stamp endTime,
            // causing the reasoning duration timer to tick indefinitely.
            const prev = getSessionState(state, agentId, sid);
            if (prev.sessionStatus?.status === "streaming" && status.status !== "streaming") {
              // Stamp endTime on any open thinking message before clearing its ID
              if (prev.thinkingMessageId) {
                const endTime = Date.now();
                sessionPatch.messages = prev.messages.map((msg) =>
                  msg.id === prev.thinkingMessageId && !msg.endTime
                    ? { ...msg, endTime }
                    : msg,
                );
              }
              sessionPatch.streamingMessageId = null;
              sessionPatch.streamBuffer = "";
              sessionPatch.isReasoning = false;
              sessionPatch.thinkingMessageId = null;
            }

            // When status transitions TO Idle from non-Idle, clear pending flags
            if (prev.sessionStatus?.status !== "idle" && status.status === "idle") {
              sessionPatch.pendingApproval = {};
              sessionPatch.pendingQuestion = null;
              sessionPatch.iterationLimitPaused = null;
            }

            // 429 retry UX: populate retryWaitInfo when paused with retry_info
            if (status.status === "paused" && status.detail?.retry_info) {
              sessionPatch.retryWaitInfo = {
                waitMs: status.detail.retry_info.wait_ms,
                attempt: status.detail.retry_info.attempt,
                maxAttempts: status.detail.retry_info.max_attempts,
                provider: status.detail.retry_info.provider,
                startedAt: Date.now(),
              };
            } else if (prev.sessionStatus?.status === "paused" && status.status !== "paused") {
              // Clear retry wait info when leaving paused state
              sessionPatch.retryWaitInfo = null;
            }

            // Update session state (model/provider/status) then agent-level defaults
            const sessionResult = updateSessionState(state, agentId, sid, sessionPatch);
            let agentStates = sessionResult.agentStates;

            if (typeof data.model === "string" && data.model) {
              agentStates = updateAgentState(
                { ...state, agentStates },
                agentId,
                { preferredModel: data.model as string },
              ).agentStates;
            }

            // Sync per-session workspace from session_state_changed event.
            // Workspace can change during session lifetime (just like model can be switched).
            if (typeof data.workspace_id === "string" && data.workspace_id) {
              useWorkspaceStore.getState().setSessionWorkspaceLocal(sid, data.workspace_id as string);
            }
            return { agentStates };
          });
        }
      }
      break;
    }

    // Todo list updated — from todo_write built-in tool
    case "todo_list_updated": {
      if (sid) {
        const todos = data.todos as TodoItem[] | undefined;
        if (todos) {
          set((state) => updateSessionState(state, agentId, sid, { todos }));
        }
      }
      break;
    }

    default:
      console.log("[ChatStore] Unknown event type:", eventType, data);
  }
}
