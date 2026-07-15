import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { ChatMessage, ContextUsageInfo, TokenUsage, ToolApprovalNeededEvent, PaginatedMessages, ConversationEntry, SessionStatus, AskQuestionEvent, ModelEntry, TodoItem } from "../lib/types";
import { useAgentStore } from "./agentStore";
import { useGatewayStore } from "./gatewayStore";
import { useUserProfileStore } from "./userProfileStore";
import { useWorkspaceStore } from "./workspaceStore";
import { getGatewayUrl } from "../lib/config";
import i18n from "../i18n";

// ---------------------------------------------------------------------------
// ADR-027: Mutable streaming content store (outside React/Zustand state).
//
// Streaming messages have a stable ChatMessage reference in React state — they
// are only appended on first appearance and removed when flushed to JSONL,
// never mutated in place. Their volatile `content` and `isStreaming` flag live
// in this external Map, read by useSyncExternalStore hooks during rendering.
// This avoids creating new ChatMessage objects on every poll when only the
// streaming content changes (~500ms polling × N messages = GC pressure).
//
// Keyed by `{sessionId}:{messageId}` to avoid cross-session collisions.
// ---------------------------------------------------------------------------

interface StreamingEntry {
  content: string;
  isStreaming: boolean;
}

const streamingContents = new Map<string, StreamingEntry>();
const streamingListeners = new Map<string, Set<() => void>>();

function streamingKey(sessionId: string, messageId: string): string {
  return `${sessionId}:${messageId}`;
}

/** Public reader for useSyncExternalStore */
export function getStreamingContent(
  sessionId: string,
  messageId: string,
): StreamingEntry | null {
  return streamingContents.get(streamingKey(sessionId, messageId)) ?? null;
}

/** Public subscriber for useSyncExternalStore */
export function subscribeStreaming(
  sessionId: string,
  messageId: string,
  callback: () => void,
): () => void {
  const key = streamingKey(sessionId, messageId);
  let set = streamingListeners.get(key);
  if (!set) {
    set = new Set();
    streamingListeners.set(key, set);
  }
  set.add(callback);
  return () => {
    set!.delete(callback);
    if (set!.size === 0) streamingListeners.delete(key);
  };
}

/** Internal: notify all useSyncExternalStore subscribers for a single entry. */
function notifyStreamingSubscribers(sessionId: string, messageId: string): void {
  const set = streamingListeners.get(streamingKey(sessionId, messageId));
  if (set) for (const cb of set) cb();
}

/** Internal: remove a streaming entry + its listeners. */
function deleteStreamingEntry(sessionId: string, messageId: string): void {
  const key = streamingKey(sessionId, messageId);
  streamingContents.delete(key);
  streamingListeners.delete(key);
}

/** Internal: clear all streaming state for a given session. */
function clearSessionStreaming(sessionId: string): void {
  const prefix = `${sessionId}:`;
  for (const key of streamingContents.keys()) {
    if (key.startsWith(prefix)) streamingListeners.delete(key);
  }
  for (const key of streamingContents.keys()) {
    if (key.startsWith(prefix)) streamingContents.delete(key);
  }
}

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
  /** ADR-014/021: Session lifecycle status from backend (sole source of truth for "sending" state) */
  sessionStatus: SessionStatus | null;
  /** Last accessed timestamp — used for LRU eviction */
  lastAccessed: number;
  /** Per-session todo list (from todo_write tool) */
  todos: TodoItem[];
  /** Per-session queued messages (typed during streaming, sent when agent becomes idle) */
  queuedMessages: string[];
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
  /** ADR-025: Whether more undelivered data exists (batch catch-up signal) */
  hasMoreIncremental: boolean;
  /** ADR-021: Per-session AbortController for cancelling in-flight loadSessionMessages */
  abortController: AbortController | null;
  /** ADR-021: Per-session load sequence number to prevent race conditions */
  loadSequence: number;
}

const DEFAULT_SESSION_STATE: SessionChatState = {
  messages: [],
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
  sessionStatus: null,
  lastAccessed: 0,
  todos: [],
  queuedMessages: [],
  model: null,
  provider: null,
  ratio: null,
  reasoningEffort: null,
  temperature: null,
  isCompacting: false,
  treeExpandedPaths: [],
  attachedContext: [],
  hasMoreIncremental: false,
  abortController: null,
  loadSequence: 0,
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
  /** Whether the MQTT connection to the Gateway broker is established (ADR-033) */
  mqttConnected: boolean;
  availableModels: ModelEntry[];
  /** Whether more messages are being loaded */
  isLoadingMore: boolean;

  // ---- Actions ----
  sendMessage: (content: string, agentId: string, command?: string, documentIds?: string[], documents?: Array<{ id: string; filename: string; format: string; size: number; path?: string }>, imageParts?: Array<{ url: string; width: number; height: number }>) => Promise<void>;
  stopCurrentMessage: (agentId: string) => Promise<void>;
  sendStop: (agentId: string) => void;
  /** Clear session state for a specific agent's active session */
  clearMessages: (agentId: string) => void;
  /** Clear a specific session's state */
  clearSessionState: (agentId: string, sessionId: string) => void;
  /** Remove a session's cached state (e.g. on session delete) */
  removeSessionState: (agentId: string, sessionId: string) => void;
  trimMessagesTo: (agentId: string, count: number) => void;
  setCurrentModel: (model: string, provider: string, agentId: string) => void;
  /** ADR-033: Send workspace switch via MQTT (per-session). */
  setSessionWorkspaceMqtt: (agentId: string, sessionId: string, workspaceId: string) => void;
  setAvailableModels: (models: ModelEntry[]) => void;
  /** Set per-session reasoning effort override (auto/off/low/medium/high) */
  setReasoningEffort: (effort: string, agentId: string) => void;
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
    /** ADR-025: when true, uses backend-managed delivery cursor */
    incremental?: boolean,
  ) => Promise<{ hasMore: boolean; noNewData: boolean } | undefined>;
  abortSessionLoad: (agentId: string, sessionId: string) => void;
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
  /** ADR-032 C4c: Send a user-initiated compression action (tool results or summary). */
  sendCompressAction: (agentId: string, sessionId: string, compressType: string) => void;
  /** Toggle a file tree directory expansion (per-session) */
  toggleTreeExpandedPath: (agentId: string, sessionId: string, relPath: string) => void;
  /**
   * Ensure all ancestor directories of `relPath` are expanded in the session's
   * file tree (additive merge with existing expansions). Idempotent. No-op for
   * `relPath === ""` or files directly at the workspace root.
   *
   * Example: for `src/components/Foo.tsx` this adds "src" and
   * "src/components" to `treeExpandedPaths` without removing other expansions.
   */
  expandTreeToPath: (agentId: string, sessionId: string, relPath: string) => void;
  /** Add a file/directory/selection to attached chat context */
  addAttachedContext: (agentId: string, sessionId: string, item: { id: string; type: "file" | "directory" | "selection"; name: string; absPath: string; startLine?: number; endLine?: number }) => void;
  /** Remove a file/directory from attached chat context */
  removeAttachedContext: (agentId: string, sessionId: string, id: string) => void;
  /** Clear all attached chat context for a session */
  clearAttachedContext: (agentId: string, sessionId: string) => void;
  /** Add a message to the per-session queue (typed during streaming) */
  addQueuedMessage: (agentId: string, sessionId: string, message: string) => void;
  /** Remove a message from the per-session queue by index */
  removeQueuedMessage: (agentId: string, sessionId: string, index: number) => void;
  /** Replace the entire per-session queue (e.g. after sending all) */
  setQueuedMessages: (agentId: string, sessionId: string, messages: string[]) => void;
  /** ADR-015 Phase 5: Pull initial session state from backend (model/provider/status/ratio/etc.) */
  fetchSessionState: (agentId: string, sessionId: string) => Promise<void>;
}

// ADR-033: Initialize MQTT event listener.
// Called once after MQTT connection is established.
// Routes structured agent-event JSON from the Rust backend to handleMessageEvent.
let _mqttUnlisten: (() => void) | null = null;

export async function initMqttListener(): Promise<void> {
  // Unregister previous listener if any
  if (_mqttUnlisten) {
    _mqttUnlisten();
    _mqttUnlisten = null;
  }

  _mqttUnlisten = await listen("agent-event", (event) => {
    const data = event.payload as Record<string, unknown>;
    const agentId = data.agent_id as string;
    if (!agentId) {
      // Events without an agent_id are ignored at the store level
      return;
    }

    const store = useChatStore;
    handleMessageEvent(data, store.setState, store.getState, agentId);
  });

  // Mark MQTT as connected
  useChatStore.setState({ mqttConnected: true });
}

export function disposeMqttListener(): void {
  if (_mqttUnlisten) {
    _mqttUnlisten();
    _mqttUnlisten = null;
  }
  useChatStore.setState({ mqttConnected: false });
}

export const useChatStore = create<ChatStore>((set, get) => ({
  agentStates: {},
  mqttConnected: false,
  availableModels: [],
  isLoadingMore: false,

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
        const updatedSessions = { ...agent.sessionStates, [sessionId]: { ...makeInitialSessionState(agent), sessionStatus: status, lastAccessed: Date.now() } };
        const updatedAgent = { ...agent, sessionStates: updatedSessions };
        return { agentStates: { ...state.agentStates, [agentId]: updatedAgent } };
      }
      return updateSessionState(state, agentId, sessionId, { sessionStatus: status });
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
          updatedSessions[sessionId] = { ...session, sessionStatus: status, lastAccessed: Date.now() };
        } else {
          // Crash restart: session not cached yet — create entry with backend status
          updatedSessions[sessionId] = {
            ...makeInitialSessionState(agent),
            sessionStatus: status,
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
    set((state) => updateSessionState(state, agentId, sessionId, { isCompacting: true }));
    // ADR-033: Send via MQTT; silently ignore if MQTT is not connected
    invoke("mqtt_publish_control", {
      agentId,
      command: "compact_context",
      payloadJson: { session_id: sessionId },
    }).catch((err: unknown) => console.warn("[ChatStore] compact_context via MQTT failed:", err));
  },

  /** ADR-032 C4c: Send a user-initiated compression action to the Runtime.
   *  ADR-033: Sent via MQTT compact_context (no dedicated compress_action in MQTT proto). */
  sendCompressAction: (agentId: string, sessionId: string, compressType: string) => {
    // ADR-034 Phase 5: Use dedicated compress_action command with compress_type
    invoke("mqtt_publish_control", {
      agentId,
      command: "compress_action",
      payloadJson: { session_id: sessionId, compress_type: compressType },
    }).catch((err: unknown) => console.warn("[ChatStore] compress_action via MQTT failed:", err));
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
        tokenUsage: null,
        contextUsage: null,
        hasMoreMessages: false,
        messageCursor: null,
        iterationLimitPaused: null,
        pendingApproval: {},
              loadError: null,
        hasMoreIncremental: false,
        abortController: null,
        loadSequence: 0,
      }),
    }));
  },

  clearSessionState: (agentId: string, sessionId: string) => {
    set((state) => ({
      ...updateSessionState(state, agentId, sessionId, {
        messages: [],
        tokenUsage: null,
        contextUsage: null,
        hasMoreMessages: false,
        messageCursor: null,
        iterationLimitPaused: null,
        pendingApproval: {},
              loadError: null,
        hasMoreIncremental: false,
        abortController: null,
        loadSequence: 0,
      }),
    }));
  },

  removeSessionState: (agentId: string, sessionId: string) => {
    clearSessionStreaming(sessionId);
    set((state) => {
      const agent = getAgentState(state, agentId);
      const newSessionStates = { ...agent.sessionStates };
      delete newSessionStates[sessionId];
      return updateAgentState(state, agentId, { sessionStates: newSessionStates });
    });
  },

  // ADR-033: connectStream removed — MQTT connection is managed by the Rust backend.
  // The frontend no longer creates WebSocket connections.

  sendMessage: async (content: string, agentId: string, command?: string, documentIds?: string[], documents?: Array<{ id: string; filename: string; format: string; size: number; path?: string }>, imageParts?: Array<{ url: string; width: number; height: number }>) => {
    const sessionId = getAgentState(get(), agentId).activeSessionId;

    // Add user message to the active session's state
    // NOTE: Use crypto.randomUUID() so the ID survives round-trip to the backend
    // and back — loadSessionMessages() deduplicates by message ID, so the
    // optimistic render and the backend-persisted message must share the same ID.
    const userMsgId = `msg-${crypto.randomUUID()}`;

    // Collect documents for optimistic render: uploaded files + attached context.
    const optimisticDocs: ChatMessage["documents"] = [];

    // Uploaded documents (via doc_reader)
    if (documents && documents.length > 0) {
      for (const doc of documents) {
        optimisticDocs.push({
          filename: doc.filename,
          format: doc.format,
          size: doc.size,
          documentId: doc.id,
        });
      }
    }

    // Attached context files (from workspace explorer / editor "Add to Chat")
    // Include them as document chips so the first render shows file icons,
    // matching the visual treatment that the backend-enriched message would
    // have had before ID-based dedup was introduced.
    if (sessionId) {
      const ss = getSessionState(get(), agentId, sessionId);
      if (ss.attachedContext.length > 0) {
        for (const ctx of ss.attachedContext) {
          if (ctx.type === "file" || ctx.type === "selection") {
            optimisticDocs.push({
              filename: ctx.name,
              format: "text",
            });
          }
        }
      }
    }

    const userMsg: ChatMessage = {
      id: userMsgId,
      type: "user",
      content,
      timestamp: Date.now(),
      ...(optimisticDocs.length > 0 ? { documents: optimisticDocs } : {}),
      ...getUserSenderInfo(),
    };

    // Attach image info to user message for inline rendering
    if (imageParts && imageParts.length > 0) {
      userMsg.imageUrls = imageParts.map((img) => img.url);
    }

    if (sessionId) {
      set((state) => ({
        ...updateSessionState(state, agentId, sessionId, {
          messages: [...getSessionState(state, agentId, sessionId).messages, userMsg],
                }),
      }));

    }

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

    // Build attached context payload from session state (files/selections from
    // workspace explorer right-click or editor "Add to Chat" button).
    // Passes file paths + line ranges as structured metadata so the Runtime
    // can assemble the enriched context and inject it into the LLM prompt.
    // The frontend no longer assembles prompt text — all context enrichment
    // (including human-readable file summaries) is done by the backend.
    let attachedContextPayload: Array<{ absPath: string; type: string; startLine?: number; endLine?: number }> | undefined;
    if (sessionId) {
      const ss = getSessionState(get(), agentId, sessionId);
      if (ss.attachedContext.length > 0) {
        attachedContextPayload = ss.attachedContext.map((ctx) => ({
          absPath: ctx.absPath,
          type: ctx.type,
          startLine: ctx.startLine,
          endLine: ctx.endLine,
        }));
      }
    }

    // Clear attached context after sending (one-shot)
    if (sessionId) {
      const ss = getSessionState(get(), agentId, sessionId);
      if (ss.attachedContext.length > 0) {
        set((s) => updateSessionState(s, agentId, sessionId, { attachedContext: [] }));
      }
    }

    // ADR-034 Phase 5: All messages sent via MQTT with params_json for rich payload.
    // HTTP fallback removed — Gateway no longer has send_message endpoint.
    const params: Record<string, unknown> = {};
    if (documentIds?.length) params.document_ids = documentIds;
    if (contentParts?.length) params.content_parts = contentParts;
    if (attachedContextPayload?.length) params.attached_context = attachedContextPayload;
    const paramsJson = Object.keys(params).length > 0 ? JSON.stringify(params) : "";

    try {
      await invoke("mqtt_publish_control", {
        agentId,
        command: "chat_message",
        payloadJson: {
          session_id: sessionId,
          message_id: userMsgId,
          content,
          command: command ?? "",
          params_json: paramsJson,
        },
      });
      console.log("[ChatStore] Message sent via MQTT:", userMsgId);
    } catch (error) {
      console.error("[ChatStore] MQTT message send failed:", error);
      const errorMsg: ChatMessage = {
        id: `msg-error-${Date.now()}`,
        type: "system",
        content: `Failed to send message: Agent may not be connected yet. Please wait and try again.`,
        timestamp: Date.now(),
      };
      if (sessionId) {
        set((state) => ({
          ...updateSessionState(state, agentId, sessionId, {
                messages: [...getSessionState(state, agentId, sessionId).messages, errorMsg],
          }),
        }));
      }
    }
  },

  stopCurrentMessage: async (agentId: string) => {
    console.log("[ChatStore] Stopping current message for agent:", agentId);

    // ADR-034 Phase 5: Send stop via MQTT with reason
    const sessionId = getAgentState(get(), agentId).activeSessionId;
    invoke("mqtt_publish_control", {
      agentId,
      command: "stop",
      payloadJson: { session_id: sessionId, reason: "user_requested" },
    }).catch((err: unknown) => console.warn("[ChatStore] stop via MQTT failed:", err));

    const activeSessionId = getAgentState(get(), agentId).activeSessionId;
    if (activeSessionId) {
      set((state) => ({
        ...updateSessionState(state, agentId, activeSessionId, {
          }),
      }));
    }
  },

  sendStop: (agentId: string) => {
    // ADR-034 Phase 5: Send stop via MQTT with reason
    const sessionId = getAgentState(get(), agentId).activeSessionId;
    invoke("mqtt_publish_control", {
      agentId,
      command: "stop",
      payloadJson: { session_id: sessionId, reason: "user_requested" },
    }).catch((err: unknown) => console.warn("[ChatStore] sendStop via MQTT failed:", err));

    // Optimistic: immediately mark as stopping so the UI exits "working" state
    // without waiting for the backend Stopped/SessionStateChanged event.
    const activeSessionId = getAgentState(get(), agentId).activeSessionId;
    if (activeSessionId) {
      set((state) =>
        updateSessionState(state, agentId, activeSessionId, {}),
      );
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

    // ADR-033: Send model switch via MQTT.
    // `provider_id` mirrors the gRPC/WebSocket-era payload field
    // (`params["provider"]` extracted by `cli.rs::process_gateway_recv`).
    // The Runtime uses it to rebuild the per-session Provider instance when
    // the user picks a model from a different provider (e.g. switching
    // deepseek-v4-flash → minimax-cn-coding-plan/MiniMax-M3). Without
    // this field, the LLM call would still target the previous provider's
    // base_url and yield 401 errors on the new model's endpoint.
    invoke("mqtt_publish_control", {
      agentId,
      command: "model_switch",
      payloadJson: { model_id: model, session_id: sessionId, provider_id: provider },
    }).catch((err: unknown) => console.warn("[ChatStore] model_switch via MQTT failed:", err));
  },

  setSessionWorkspaceMqtt: (agentId: string, sessionId: string, workspaceId: string) => {
    invoke("mqtt_publish_control", {
      agentId,
      command: "workspace_switch",
      payloadJson: { workspace_id: workspaceId, session_id: sessionId },
    }).catch((err: unknown) => console.warn("[ChatStore] workspace_switch via MQTT failed:", err));
  },
  setReasoningEffort: (effort: string, agentId: string) => {
    const sessionId = getAgentState(get(), agentId).activeSessionId;
    if (!sessionId) return;

    // Optimistically update frontend state (Runtime will confirm)
    set((state) => updateSessionState(state, agentId, sessionId, { reasoningEffort: effort }));

    // ADR-033: Send reasoning effort via MQTT
    invoke("mqtt_publish_control", {
      agentId,
      command: "reasoning_effort",
      payloadJson: { effort, session_id: sessionId },
    }).catch((err: unknown) => console.warn("[ChatStore] reasoning_effort via MQTT failed:", err));
  },
  setAvailableModels: (models: ModelEntry[]) => {
    set({ availableModels: models });
  },
  continueExecution: async (agentId: string) => {
    try {
      const sessionId = getAgentState(get(), agentId).activeSessionId;
      await invoke("mqtt_publish_control", {
        agentId,
        command: "continue_execution",
        payloadJson: {
          session_id: sessionId ?? "",
          reason: "user_requested",
        },
      });
      if (sessionId) {
        set((state) => ({
          ...updateSessionState(state, agentId, sessionId, { iterationLimitPaused: null }),
        }));
      }
    } catch (error) {
      console.error("[ChatStore] Failed to send continue signal:", error);
    }
  },

  publishUpdateSessionTitle: (agentId: string, sessionId: string, title: string) => {
    invoke("mqtt_publish_control", {
      agentId,
      command: "update_session_title",
      payloadJson: { session_id: sessionId, title },
    }).catch((err: unknown) => console.warn("[ChatStore] update_session_title via MQTT failed:", err));
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
    incremental?: boolean,
  ): Promise<{ hasMore: boolean; noNewData: boolean } | undefined> => {
    // ADR-021: Per-session abortController + loadSequence (no cross-session interference).
    const sessionState = getSessionState(get(), agentId, sessionId);
    const seq = sessionState.loadSequence + 1;

    const oldController = sessionState.abortController;
    if (oldController) {
      oldController.abort();
    }
    const controller = new AbortController();
    set((state) => ({
      ...updateSessionState(state, agentId, sessionId, {
        loadSequence: seq,
        abortController: controller,
      }),
    }));

    // Only show loading indicator for initial loads (no cursor, not incremental)
    const isIncremental = !!(cursor || incremental);
    if (!isIncremental) {
      set((state) => ({
        ...updateSessionState(state, agentId, sessionId, { isLoadingSession: true, loadError: null }),
      }));
    }

    try {
      const params = new URLSearchParams();
      params.set("limit", String(limit));
      params.set("direction", direction);
      if (cursor) params.set("cursor", cursor);
      // ADR-025: incremental flag for backend-managed delivery cursor
      if (incremental) params.set("incremental", "true");

      const resp = await fetch(
        `${getGatewayUrl()}/api/agents/${agentId}/sessions/${sessionId}/messages?${params}`,
        { signal: controller.signal },
      );

      if (getSessionState(get(), agentId, sessionId).loadSequence !== seq) {
        console.log(`[ChatStore] Discarding stale loadSessionMessages response (seq ${seq})`);
        return;
      }

      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);

      const data = (await resp.json()) as PaginatedMessages;

      if (getSessionState(get(), agentId, sessionId).loadSequence !== seq) {
        console.log(`[ChatStore] Discarding stale response after json parse (seq ${seq})`);
        return;
      }

      console.log(`[ChatStore:DEBUG] Loaded ${data.messages?.length ?? 0} messages for session ${sessionId}${incremental ? " (incremental)" : ""}, has_more=${data.has_more}${data.messages ? `, streaming=${data.messages.some(m => m.is_streaming)}` : ""}`);
      if (data.messages && data.messages.length > 0) {
        const lastMsg = data.messages[data.messages.length - 1];
        console.log(`[ChatStore:DEBUG] Last incremental message: id=${lastMsg.id}, role=${lastMsg.role}, contentPreview=${typeof lastMsg.content === 'string' ? lastMsg.content.slice(0, 80) : String(lastMsg.content).slice(0, 80)}`);
      }

      const converted = mergeDocumentUploads(data.messages ?? [], agentId);

      // ADR-025/027: Compute poll result for PollingManager.
      // With mutable streaming content, noNewData = true only when:
      //  1. No new settled messages appeared, AND
      //  2. No streaming content changed (compare against mutable store)
      const pollResult: { hasMore: boolean; noNewData: boolean } | undefined =
        incremental
          ? (() => {
              const hasStreamingChanged = converted.some((m) => {
                if (!m.isStreaming) return false;
                const prev = streamingContents.get(
                  streamingKey(sessionId, m.id),
                );
                return !prev || prev.content !== m.content;
              });
              const hasNewSettled = converted.some((m) => !m.isStreaming);
              return {
                hasMore: data.has_more ?? false,
                noNewData: !hasNewSettled && !hasStreamingChanged,
              };
            })()
          : undefined;

      set((state) => {
        const ss = getSessionState(state, agentId, sessionId);
        if (ss.loadSequence !== seq) {
          console.log(`[ChatStore] Discarding state update — sequence changed`);
          return {};
        }

        // ADR-027: Incremental poll — mutable streaming content.
        //
        // Streaming messages (is_streaming=true, id="streaming:{line}")
        // are projected into messages[] by the Gateway. Their volatile
        // content lives in the external `streamingContents` Map, NOT in
        // React state. This avoids creating new ChatMessage objects on
        // every poll, eliminating GC pressure from ~500ms polling cycles.
        //
        // Merge flow:
        //   1. Streaming messages → update mutable Map; append to
        //      messages[] only on first appearance (ChatMessage ref
        //      stays stable across polls)
        //   2. Settled messages → normal upsert by id
        //   3. Stale streaming ids absent from poll → delete from Map +
        //      remove from messages[]
        //   4. Sort: non-streaming first (insertion order), streaming last
        //   5. If nothing changed (no new settled, streaming content
        //      unchanged) → return {} — zero allocations this poll cycle
        if (incremental) {
          const ss = getSessionState(state, agentId, sessionId);
          const existingById = new Map(ss.messages.map((m) => [m.id, m]));
          const respIds = new Set<string>();
          let messagesChanged = false;

          for (const m of converted) {
            respIds.add(m.id);
            if (m.isStreaming) {
              // Streaming: update mutable store, React state only on first sight
              const prev = streamingContents.get(streamingKey(sessionId, m.id));
              if (!prev || prev.content !== m.content) {
                streamingContents.set(streamingKey(sessionId, m.id), {
                  content: m.content,
                  isStreaming: true,
                });
                notifyStreamingSubscribers(sessionId, m.id);
              }
              if (!existingById.has(m.id)) {
                // First appearance — add a shell message (content="" — the
                // real content comes from useStreamingContent in the renderer)
                existingById.set(m.id, {
                  ...m,
                  content: "",
                  isStreaming: true,
                } as ChatMessage);
                messagesChanged = true;
              }
              // Already exists → ChatMessage ref unchanged, React skips
              // reconciliation. Content changes are pushed via
              // useSyncExternalStore → granular per-message re-renders.
            } else {
              // Settled message: normal upsert
              const existing = existingById.get(m.id);
              if (!existing || existing.content !== m.content || existing.type !== m.type) {
                existingById.set(m.id, m);
                messagesChanged = true;
              }
            }
          }

          // Cleanup: streaming ids absent from this poll → flushed to JSONL
          for (const [id, m] of existingById) {
            if (m.isStreaming && !respIds.has(id)) {
              deleteStreamingEntry(sessionId, id);
              existingById.delete(id);
              messagesChanged = true;
            }
          }

          if (!messagesChanged) {
            // Zero allocations — content changes were handled by
            // useSyncExternalStore notifications without touching React state
            return {};
          }

          // Stable sort: settled first, streaming last
          const settled: ChatMessage[] = [];
          const streaming: ChatMessage[] = [];
          for (const m of existingById.values()) {
            (m.isStreaming ? streaming : settled).push(m);
          }
          const messages = [...settled, ...streaming];

          return {
            ...updateSessionState(state, agentId, sessionId, {
              messages,
              hasMoreIncremental: data.has_more ?? false,
              isLoadingSession: false,
              loadError: null,
            }),
            isLoadingMore: false,
          };
        }

        // Cursor-based pagination (load more history)
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

        // Full initial load — replace all messages + reset streaming state
        clearSessionStreaming(sessionId);
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

      return pollResult;
    } catch (e: unknown) {
      if (getSessionState(get(), agentId, sessionId).loadSequence !== seq) {
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
      const currentController = getSessionState(get(), agentId, sessionId).abortController;
      if (currentController === controller) {
        set((state) => ({
          ...updateSessionState(state, agentId, sessionId, { abortController: null }),
        }));
      }
    }
  },

  abortSessionLoad: (agentId: string, sessionId: string) => {
    const controller = getSessionState(get(), agentId, sessionId).abortController;
    if (controller) {
      controller.abort();
      set((state) => ({
        ...updateSessionState(state, agentId, sessionId, { abortController: null }),
      }));
    }
    set((state) => ({
      ...updateSessionState(state, agentId, sessionId, {
        loadSequence: getSessionState(state, agentId, sessionId).loadSequence + 1,
      }),
    }));
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

  expandTreeToPath: (agentId, sessionId, relPath) => {
    if (!relPath) return;
    const parts = relPath.split("/");
    if (parts.length <= 1) return;
    set((state) => {
      const ss = getSessionState(state, agentId, sessionId);
      const current = ss.treeExpandedPaths;
      const set_ = new Set(current);
      let changed = false;
      for (let i = 0; i < parts.length - 1; i++) {
        const ancestor = parts.slice(0, i + 1).join("/");
        if (!set_.has(ancestor)) {
          set_.add(ancestor);
          changed = true;
        }
      }
      if (!changed) return state;
      return updateSessionState(state, agentId, sessionId, {
        treeExpandedPaths: Array.from(set_),
      });
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

  addQueuedMessage: (agentId: string, sessionId: string, message: string) => {
    set((state) => {
      const ss = getSessionState(state, agentId, sessionId);
      return updateSessionState(state, agentId, sessionId, {
        queuedMessages: [...ss.queuedMessages, message],
      });
    });
  },

  removeQueuedMessage: (agentId: string, sessionId: string, index: number) => {
    set((state) => {
      const ss = getSessionState(state, agentId, sessionId);
      return updateSessionState(state, agentId, sessionId, {
        queuedMessages: ss.queuedMessages.filter((_, i) => i !== index),
      });
    });
  },

  setQueuedMessages: (agentId: string, sessionId: string, messages: string[]) => {
    set((state) => updateSessionState(state, agentId, sessionId, { queuedMessages: messages }));
  },

  // ADR-015 Phase 5: Pull initial session state from backend.
  // Maps the /api/agents/{id}/sessions/{sid}/state response to SessionChatState fields.
  // Errors are non-fatal — warns and returns without blocking startup.
  fetchSessionState: async (agentId: string, sessionId: string) => {
    try {
      const resp = await fetch(
        `${getGatewayUrl()}/api/agents/${agentId}/sessions/${sessionId}`,
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
        todos?: TodoItem[] | null;
        context_usage?: ContextUsageInfo | null;
      };
      const sessionPatch: Partial<SessionChatState> = {};
      if (typeof data.model === "string" && data.model) sessionPatch.model = data.model;
      if (typeof data.provider === "string" && data.provider) sessionPatch.provider = data.provider;
      if (typeof data.ratio === "number") sessionPatch.ratio = data.ratio;
      if (typeof data.reasoning_effort === "string" && data.reasoning_effort) sessionPatch.reasoningEffort = data.reasoning_effort;
      if (typeof data.temperature === "number") sessionPatch.temperature = data.temperature;
      if (data.todos && Array.isArray(data.todos)) {
        sessionPatch.todos = data.todos as TodoItem[];
      }
      if (data.context_usage && typeof data.context_usage === "object") {
        sessionPatch.contextUsage = data.context_usage as ContextUsageInfo;
      }
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
    isStreaming: entry.is_streaming,
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

    // ── Strip attached-context enrichment from user messages ──────────
    // Frontend prepends "[Attached context:]\n- file: `path`\n\n" to the
    // user content; backend then appends "\n\nThe following workspace
    // files were attached..." enrichment.  Reconstruct the `documents`
    // array from the file references and keep only the actual user input.
    // `selection` is emitted by the backend when an editor line-range was
    // attached (chatStore `attachedContext.type === "selection"`); treat
    // it the same as `file` for chip/document reconstruction — the line
    // range in the line itself is informational only.
    if (msg.type === "user" && msg.content) {
      let cleanedContent = msg.content;
      let attachedFiles: ChatMessage["documents"] = [];

      // Parse frontend-added [Attached context:] block
      const attachedCtxMatch = cleanedContent.match(
        /^\[Attached context:\]\n([\s\S]*?)(?:\n\n|$)/,
      );
      if (attachedCtxMatch) {
        const block = attachedCtxMatch[1];
        for (const line of block.split('\n')) {
          const fileMatch = line.match(/^- (?:file|folder|selection): `(.+?)`/);
          if (fileMatch) {
            const absPath = fileMatch[1];
            const filename =
              absPath.replace(/[/\\]$/, '').split(/[/\\]/).pop() ?? absPath;
            attachedFiles.push({ filename, format: 'text' });
          }
        }
        // Remove the [Attached context:] block
        cleanedContent = cleanedContent.slice(attachedCtxMatch[0].length);
      }

      // Strip backend-added "The following workspace files..." enrichment
      cleanedContent = cleanedContent.replace(
        /\n\nThe following workspace files were attached by the user\..*$/s,
        '',
      );

      // Apply changes only if we found enrichment text
      if (cleanedContent !== msg.content) {
        msg.content = cleanedContent;
        if (attachedFiles.length > 0) {
          msg.documents = [...(msg.documents ?? []), ...attachedFiles];
        }
      }
    }

    result.push(msg);
  }

  return result;
}

// ── WebSocket event handler — routes by event.session_id ──────────────

const CONTENT_EVENT_TYPES = new Set([
  "done", "error", "tool_approval_needed", "ask_question", "iteration_limit_paused",
  "context_usage", "session_state_changed", "stopped", "todo_list_updated",
  "compacting_started", "compacting_ended", "model_confirmed", "reasoning_effort_confirmed",
  "new_data_available",
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

    // ADR-021: new_data_available — triggers HTTP poll for incremental message data.
    case "new_data_available": {
      if (!sid) break;
      const intervalMs = (data.interval_ms as number) ?? undefined;
      // DEBUG: log session state when new_data_available arrives
      const state = get();
      const ss = getSessionState(state, agentId, sid!);
      console.log(
        `[ChatStore:DEBUG] new_data_available for ${agentId}/${sid}: ` +
        `interval_ms=${intervalMs}, ` +
        `status=${ss.sessionStatus?.status}, ` +
        `messageCount=${ss.messages.length}, ` +
        `isLoadingSession=${ss.isLoadingSession}, ` +
        `loadSequence=${ss.loadSequence}`,
      );
      // ADR-025: Pure signal — no totalLines. Backend maintains delivery cursor.
      // Import PollingManager dynamically to avoid circular dependency
      import("../lib/polling").then(({ notifyNewData }) => {
        notifyNewData(agentId, sid!, intervalMs);
      }).catch((e) => {
        console.warn("[ChatStore] Failed to import PollingManager:", e);
      });
      // Update session title from backend async summarization
      const title = data.title as string | undefined;
      if (title) {
        useAgentStore.getState().updateSessionTitle(sid!, title);
      }
      break;
    }

    case "done": {
      if (!sid) break;
      const usage = data.usage as TokenUsage | undefined;

      // ADR-025: The done event marks that the backend has finished
      // processing the run.  Do NOT start a separate incremental poll
      // here — that was the root cause of the missing-segments bug.
      //
      // Starting a fire-and-forget poll here caused a race: it
      // increments loadSequence, aborts an in-flight doPoll, and its
      // own response may arrive before the backend has flushed the last
      // JSONL lines.  When streaming is null in that response, ALL
      // streaming placeholders are dropped — but their JSONL
      // replacements haven't arrived yet, so the last few
      // toolcall/thought/assistant segments vanish.
      //
      // Instead, session_state_changed → idle does a single definitive
      // final incremental poll before stopping the poll cycle.  Idle
      // guarantees the backend has flushed everything, so the final
      // poll always gets complete data.
      set((state) => {
        const ss = getSessionState(state, agentId, sid!);
        return {
          ...updateSessionState(state, agentId, sid!, {
            tokenUsage: usage ?? ss.tokenUsage,
            isCompacting: false,
          }),
        };
      });
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
      // ADR-021: Stop polling on error
      import("../lib/polling").then(({ stopPolling }) => {
        stopPolling(agentId, sid!);
      }).catch(() => {});
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
          isCompacting: false,
          }),
      }));
      break;
    }

    case "stopped": {
      if (!sid) break;
      // ADR-021: Stop polling on user stop
      import("../lib/polling").then(({ stopPolling }) => {
        stopPolling(agentId, sid!);
      }).catch(() => {});
      set((state) => ({
        ...updateSessionState(state, agentId, sid!, {
          isCompacting: false,
          }),
      }));
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
        // ADR-025: One-shot poll to fetch the compaction record. No
        // coordinates — backend delivery cursor handles it.
        get().loadSessionMessages(
          agentId, sid, undefined, 50, "backward",
          true, // incremental = true (ADR-025)
        );
      }
      break;

    case "embedding_migration_progress": {
      // Forward migration progress from WebSocket to gatewayStore.
      // agentId comes from the per-agent WebSocket connection context.
      const processed = data.processed as number;
      const total = data.total as number;
      if (processed != null && total != null) {
        useGatewayStore.getState().updateMigrationProgress(agentId, processed, total);
      }
      break;
    }

    case "context_usage": {
      if (sid) {
        // ADR-033: Runtime now publishes a fully-populated `ContextUsageInfo`
        // under `data.context_usage` (serialised from `ContextUsagePayload.context_usage_json`).
        // The top-level `input_tokens` / `output_tokens` / `total_*_tokens` fields
        // are legacy per-turn counts only and must NOT be used to populate
        // `ContextUsageInfo` — they lack `context_window` / `total_tokens` /
        // `usage_percent` / `usable_context`, which the StatusBar reads.
        const nested = (data as Record<string, unknown>).context_usage;
        const usage: ContextUsageInfo | null =
          nested && typeof nested === "object"
            ? (nested as ContextUsageInfo)
            : null;
        if (usage) {
          console.log("[ChatStore] context_usage RECEIVED for agent:", agentId, usage);
          set((state) => updateSessionState(state, agentId, sid, { contextUsage: usage, isCompacting: false }));
        } else {
          console.warn(
            "[ChatStore] context_usage event missing nested context_usage payload — skipping StatusBar update to avoid undefined fields:",
            data,
          );
        }
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
          // DEBUG: log status transitions
          const prev = getSessionState(get(), agentId, sid!);
          console.log(
            `[ChatStore:DEBUG] session_state_changed for ${agentId}/${sid}: ` +
            `prev=${prev.sessionStatus?.status} → next=${status.status}, ` +
            `messageCount=${prev.messages.length}`,
          );
          set((state) => {
            const sessionPatch: Partial<SessionChatState> = { sessionStatus: status };

            // ADR-012: Backend includes per-session model/provider (from JSONL metadata).
            if (typeof data.model === "string") sessionPatch.model = data.model as string;
            if (typeof data.provider === "string") sessionPatch.provider = data.provider as string;
            // Model chars/token ratio from API calibration (for status panel display).
            if (typeof data.ratio === "number") sessionPatch.ratio = data.ratio as number;
            // Reasoning effort level (thinking level) from Runtime session state.
            if (typeof data.reasoning_effort === "string") sessionPatch.reasoningEffort = data.reasoning_effort as string;
            // Temperature override from Runtime session state.
            if (typeof data.temperature === "number") sessionPatch.temperature = data.temperature as number;
            // ADR-028: Context usage snapshot from persisted session tokens.
            // The backend includes this on session activation/resume so the
            // frontend can show token counts before the first LLM call
            // triggers a dedicated context_usage event.
            if (data.context_usage && typeof data.context_usage === "object") {
              sessionPatch.contextUsage = data.context_usage as ContextUsageInfo;
            }

            // ADR-021: Start/stop polling based on status transitions.
            // When entering streaming/waiting_approval/paused → start polling.
            // When leaving these states → stop polling.
            const prev = getSessionState(state, agentId, sid);
            const prevActive = prev.sessionStatus?.status === "streaming"
              || prev.sessionStatus?.status === "waiting_approval"
              || prev.sessionStatus?.status === "paused";
            const nextActive = status.status === "streaming"
              || status.status === "waiting_approval"
              || status.status === "paused";

            if (!prevActive && nextActive) {
              import("../lib/polling").then(({ startPolling }) => {
                startPolling(agentId, sid);
              }).catch(() => {});
            } else if (prevActive && !nextActive) {
               // ADR-025: Session transitioned out of active states.
               // Do one final incremental poll now that the backend has
               // truly finished (status → idle means all JSONL lines are
               // flushed).  This is the definitive last poll — it
               // replaces the old done-handler fire-and-forget poll that
               // caused the race.
               //
               // Done no longer starts polls; idle is the single point
               // where we (a) capture remaining data and (b) stop polling.
               get().loadSessionMessages(agentId, sid, undefined, 50, "backward", true).finally(() => {
                 import("../lib/polling").then(({ stopPolling }) => {
                   stopPolling(agentId, sid);
                 }).catch(() => {});
               });
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

    // Session lifecycle (Runtime → Desktop, ADR-033 Phase 4).
    // Backend publishes `acowork/agents/{id}/sessions/{created|deleted}`
    // protobuf events whenever a session is created or destroyed (close/delete).
    // We refresh the session list so the UI reflects the change immediately,
    // and auto-activate a freshly created session so the user's "+" button click
    // lands them on the new chat without a follow-up click.
    case "session_created": {
      const newSessionId = data.session_id as string | undefined;
      if (!newSessionId) break;
      const agentStore = useAgentStore.getState();
      // Refresh the list first so the new entry is rendered.
      agentStore.fetchSessions(agentId).catch((e) => {
        console.warn("[ChatStore] fetchSessions after session_created failed:", e);
      });
      // Auto-activate the new session — matches the "+" button UX where the
      // user expects to land on the empty new chat immediately.
      agentStore.switchSession(newSessionId, agentId);
      break;
    }

    case "session_deleted": {
      // Refresh the session list so the deleted entry disappears from the UI.
      const deletedSessionId = data.session_id as string | undefined;
      if (!deletedSessionId) break;
      const agentStore = useAgentStore.getState();
      agentStore.fetchSessions(agentId).catch((e) => {
        console.warn("[ChatStore] fetchSessions after session_deleted failed:", e);
      });
      break;
    }

    default:
      console.log("[ChatStore] Unknown event type:", eventType, data);
  }
}
