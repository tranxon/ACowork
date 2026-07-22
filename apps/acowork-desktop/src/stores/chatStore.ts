import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { ChatMessage, ContextUsageInfo, TokenUsage, ToolApprovalNeededEvent, PaginatedMessages, ConversationEntry, SessionStatus, AskQuestionEvent, ModelEntry, TodoItem, ActiveStream } from "../lib/types";
import { useAgentStore } from "./agentStore";
import { useGatewayStore } from "./gatewayStore";
import { useUserProfileStore } from "./userProfileStore";
import { useWorkspaceStore } from "./workspaceStore";
import { getGatewayUrl } from "../lib/config";
import { emitAgentConfigRefresh } from "../lib/refresh";
import i18n from "../i18n";
import { showToast } from "../components/common/ToastProvider";

// ---------------------------------------------------------------------------
// ADR-035: per-session active stream buffer (replaces ADR-027 multi-buffer).
// Keyed by sessionId. useStreamingContent reads from here exclusively.
// ---------------------------------------------------------------------------

interface StreamingEntry {
  content: string;
  isStreaming: boolean;
  /** Number of streaming lines accumulated so far (assistant or thought).
   *  Drives the "replying" indicator on long assistant streams
   *  (see MessageBubble.ASSISTANT_REPLYING_THRESHOLD). */
  lineCount: number;
}

// ADR-035 D3: StreamLine / ActiveStream types now live in lib/types.ts.
const activeStreams = new Map<string, ActiveStream>();
const streamingListeners = new Map<string, Set<() => void>>();

function notifyActiveStreamSubscribers(sessionId: string, messageId: string): void {
  const set = streamingListeners.get(`${sessionId}:${messageId}`);
  if (set) for (const cb of set) cb();
}

/**
 * Per-session snapshot cache for useSyncExternalStore.
 *
 * `getStreamingContent` MUST return a stable reference between mutations,
 * otherwise React's `useSyncExternalStore` Object.is check sees a "new"
 * value on every call and enters an infinite re-render loop
 * ("The result of getSnapshot should be cached to avoid an infinite loop"
 * → "Maximum update depth exceeded").
 *
 * Lifecycle:
 *   - `stream_delta` handler rebuilds the entry on every push so React sees
 *     a fresh reference ONLY when content actually changed, then notifies.
 *   - `record_complete` deletes the activeStream; the next `getSnapshot`
 *     call returns null, which is itself a different reference and correctly
 *     triggers the final re-render.
 *   - `clearSessionStreaming` evicts the cache entry alongside the
 *     activeStream so stale entries don't leak after a session switch.
 */
const streamingSnapshots = new Map<string, StreamingEntry>();

export function getStreamingContent(sessionId: string, messageId: string): StreamingEntry | null {
  const as = activeStreams.get(sessionId);
  if (as && as.messageId === messageId) {
    let entry = streamingSnapshots.get(sessionId);
    if (!entry) {
      entry = {
        content: as.lines.map(l => l.content).join("\n"),
        isStreaming: true,
        lineCount: as.lines.length,
      };
      streamingSnapshots.set(sessionId, entry);
    }
    return entry;
  }
  // Active stream gone (record_complete or session evicted) — drop the
  // cached snapshot too so a stale entry can't be returned if a NEW stream
  // for the same session later starts without first populating the cache.
  streamingSnapshots.delete(sessionId);
  return null;
}

export function subscribeStreaming(sessionId: string, messageId: string, callback: () => void): () => void {
  const key = `${sessionId}:${messageId}`;
  let set = streamingListeners.get(key);
  if (!set) { set = new Set(); streamingListeners.set(key, set); }
  set.add(callback);
  return () => { set!.delete(callback); if (set!.size === 0) streamingListeners.delete(key); };
}

function clearSessionStreaming(sessionId: string): void {
  activeStreams.delete(sessionId);
  // Drop the cached snapshot too — otherwise the next getSnapshot for this
  // session (e.g. a brand-new stream for the same session id) would lazily
  // notice `as` is missing and delete the entry on its own, but in the
  // window between the evict and the next getSnapshot call we'd hand out a
  // stale reference pointing at the OLD stream's content.
  streamingSnapshots.delete(sessionId);
}

// ── ADR-035 O1: messages[] cache window ──
//
// MESSAGE_CACHE_WINDOW is NOT a hard conversation limit — sessions may have
// thousands of raw entries; the user can always scroll backward through all
// of them via HTTP pagination. This window is a frontend memory
// optimization: keep at most 150 raw entries in React state to bound DOM
// nodes and render cost. Scroll-back slides the window forward (drops
// newest from the back to make room for older at the front); real-time
// MQTT appends slide it forward naturally (drops oldest from the front).
//
// Counted in raw-entry units (same as backend `PaginatedMessages.limit`)
// — a single display group can occupy multiple slots here, which is fine:
// the cache only bounds memory, the UI rendering handles folding.
const MESSAGE_CACHE_WINDOW = 150; // 3 pages of 50

/** Trim oldest entries from the front — keep the last `cap` (newest).
 *  Used for initial load and real-time MQTT appends. */
function trimOldest(messages: ChatMessage[]): ChatMessage[] {
  if (messages.length <= MESSAGE_CACHE_WINDOW) return messages;
  return messages.slice(-MESSAGE_CACHE_WINDOW);
}

/**
 * Apply `trimOldest` and adjust pagination metadata (messageOffset /
 * messageLimit / messageTotal) to keep `hasOlder` / `hasNewer` accurate
 * after mutations that bypass `loadSessionMessages` (i.e. MQTT-driven
 * inserts and optimistic user-message appends).
 *
 * The offset/limit/total model:
 *   offset = messages NEWER than the cache window (0 = includes newest)
 *   limit  = size of the cache window
 *   total  = total messages in the conversation
 *
 * When a NEW entry is added:
 *   - total += 1
 *   - If offset == 0 (cache includes newest): limit += 1, then subtract any
 *     messages dropped by trimming.  The window grew on the newer end and
 *     shrank on the older end.
 *   - If offset > 0 (user scrolled up): offset += 1 (one more message is
 *     now above the window), limit unchanged minus any trimming.
 *
 * When an IN-PLACE update occurs (same id, e.g. placeholder -> freeze):
 *   - No change to total (count unchanged).
 *   - If trimming drops messages (shouldn't normally happen since the array
 *     length is the same), reduce limit accordingly.
 */
function applyTrimAndAdjustMeta(
  ss: SessionChatState,
  newMessages: ChatMessage[],
  isNewEntry: boolean,
): Partial<SessionChatState> {
  const trimmed = trimOldest(newMessages);
  const dropped = newMessages.length - trimmed.length;
  const patch: Partial<SessionChatState> = { messages: trimmed };
  if (isNewEntry) {
    patch.messageTotal = ss.messageTotal + 1;
    if (ss.messageOffset === 0) {
      patch.messageLimit = Math.max(0, ss.messageLimit + 1 - dropped);
    } else {
      patch.messageOffset = ss.messageOffset + 1;
      patch.messageLimit = Math.max(0, ss.messageLimit - dropped);
    }
  } else if (dropped > 0) {
    patch.messageLimit = Math.max(0, ss.messageLimit - dropped);
  }
  return patch;
}

// ── ADR-035 C2: assistant activeStream safety valve ──
//
// assistant content is NEVER truncated for display — the user must see the
// full reply. But if record_complete is lost (QoS edge case) AND the idle
// realignment fallback also fails (e.g. session closed mid-stream), the
// activeStream.lines array would grow unbounded and leak memory.
//
// This cap is a SAFETY VALVE only — set far above any realistic assistant
// reply (10k lines ≈ 500k chars ≈ a small novel). Normal replies are well
// under 1k lines. If this cap ever triggers, it indicates a bug in the
// record_complete delivery path; a warning is logged for diagnosis.
//
// thought lines are already capped at 5 (D9.1), so this only applies to
// assistant. When triggered, we keep the NEWEST lines (slice(-N)) so the
// end of the reply is preserved; the oldest lines are dropped as they are
// the least likely to be the user's focus.
const ASSISTANT_LINE_SAFETY_CAP = 10_000;

/** Minimum number of accumulated streaming lines before the per-session
 *  `isAssistantReplying` flag flips true. Mirrors
 *  `MessageBubble.ASSISTANT_REPLYING_THRESHOLD`; kept here as a chatStore
 *  constant so the streaming pipeline can flip the flag without round-
 *  tripping through the React layer. The two values must stay in sync. */
const ASSISTANT_REPLYING_LINE_THRESHOLD = 3;

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
  /**
   * Pagination window coordinates returned by the last /messages HTTP
   * response. Both `messageOffset` and `messageLimit` are measured in
   * **raw entries** (one JSONL line each — a single user / assistant /
   * thought / tool_call / tool_result row), exactly like the backend
   * `PaginatedMessages` response. Display-group collapsing
   * (think + tool_call + tool_result → one chip) is the `MessageBlock`
   * abstraction local to `ChatPanel.tsx` and never reaches this state.
   *
   * Direction is **derived from these**, not stored separately:
   *
   * - `messageOffset == 0`               → window touches the newest entry.
   * - `messageOffset + messageLimit < messageTotal` → there are older entries
   *   beyond this window (scroll-up can load them).
   * - `messageOffset > 0`                → there are newer entries below the
   *   window (scroll-down can load them).
   *
   * Initial load sets `messageOffset = 0` and `messageLimit = 0`.
   * See `PaginatedMessages` in `lib/types.ts` for the request/response shape.
   */
  messageOffset: number;
  messageLimit: number;
  messageTotal: number;
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
  pendingQuestions: AskQuestionEvent[];
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
  /**
   * True while the assistant is actively streaming AND has already
   * accumulated more than `ASSISTANT_REPLYING_THRESHOLD` lines. Drives the
   * standalone "replying" indicator rendered OUTSIDE the message list by
   * ChatPanel — kept as a session-level flag (rather than derived from the
   * per-message `useStreamingContent` hook) so the indicator is independent
   * of any single MessageBubble's render lifecycle and can never get stuck
   * "on" after record_complete.
   */
  isAssistantReplying: boolean;
  /** Whether the agent is currently in reasoning/thinking phase */
  isReasoning: boolean;
  /**
   * ADR-038: Session lifecycle readiness. Set true when the Runtime
   * publishes `session_opened` (the session is confirmed Active in memory).
   * Set false initially and on `session_not_opened`. Input box / send button
   * gate on `isSessionReady` so the user can never send a `chat_message`
   * to a Closed or NotFound session — the gate is enforced client-side
   * and the Runtime also enforces it server-side via state guard.
   */
  /** Per-session loading-more flag. Guards against concurrent pagination
   *  requests (scroll-up / ensureLatestInCache) for THIS session only -
   *  other sessions are unaffected. Previously a global boolean which
   *  caused cross-session blocking. */
  isLoadingMore: boolean;
  isSessionReady: boolean;
}

const DEFAULT_SESSION_STATE: SessionChatState = {
  messages: [],
  tokenUsage: null,
  contextUsage: null,
  messageOffset: 0,
  messageLimit: 0,
  messageTotal: 0,
  iterationLimitPaused: null,
  retryWaitInfo: null,
  pendingApproval: {},
  pendingQuestions: [],
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
  isAssistantReplying: false,
  isReasoning: false,
  isSessionReady: false,
  isLoadingMore: false,
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

/** Evict oldest/unused sessions when cache exceeds MAX_CACHED_SESSIONS.
// NOTE: kept as a reference implementation for future eviction needs;
// currently dead because (a) caches are agent-scoped and capped at
// MAX_OPEN_TABS = 32 by `openSession`, and (b) `closeTab` reuses the
// `sessionStates[id]` slot by flipping `isSessionReady = false` rather
// than deleting it (preserves scroll position on re-open).  If a caller
// ever needs to bound memory growth from huge history loads, call this
// from the appropriate action — its semantics have not changed.
*/
export function _evictStaleSessions(
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
    // Release the evicted session's streaming buffer (max 10k lines ~= 1MB)
    // and its listener sets.  Without this, evicting a session that's mid-stream
    // would leak the ActiveStream entry AND every per-message Set<callback> in
    // `streamingListeners` — the messages are gone from state but the external
    // Maps still hold references, growing unboundedly with each evict+stream
    // overlap.
    clearSessionStreaming(id);
    for (const key of streamingListeners.keys()) {
      if (key.startsWith(`${id}:`)) streamingListeners.delete(key);
    }
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
  /**
   * Whether the MQTT connection to the Gateway broker is currently healthy.
   *
   * ADR-036: this field is the *consumer* output of the Rust
   * `mqtt-status` event.  The Rust eventloop is the source-of-truth;
   * the frontend never writes to this flag except for the initial
   * `false` default and the cleanup-on-dispose.
   */
  mqttConnected: boolean;
  /**
   * Most recent disconnect reason reported by the Rust eventloop, or
   * `null` while connected / before the first connection.  Surfaced in
   * the UI status bar so the user can tell *why* the input box went
   * disabled.  See ADR-036.
   */
  lastMqttError: string | null;
  availableModels: ModelEntry[];

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
  resolveQuestion: (agentId: string, requestId: string) => void;
  loadConversationHistory: (agentId: string) => Promise<void>;
  /**
   * Load messages for a session via HTTP pagination.
   * - `offset === undefined` (default): initial load — fetches the newest `limit` raw entries.
   * - `offset > 0`: paginated load — fetches `limit` raw entries older than the current window.
   *
   * Both `offset` and `limit` are in **raw-entry units** (one JSONL line
   * each — see `PaginatedMessages` in `lib/types.ts`). Display-group
   * collapsing is handled locally in `ChatPanel.messageBlocks` (the
   * `MessageBlock` strict intermediate layer) and is never transmitted.
   *
   * Direction is **derived** from `sessionState.messageOffset` after the response
   * lands (see `SessionChatState` for the rules); callers don't need to specify it.
   *
   * ADR-041 C2: `options.evictionDirection` lets the caller override the
   * default trim direction. `'none'` skips trimming entirely (used when
   * loading older messages during streaming, so the streaming tail isn't
   * evicted). When omitted, the direction is derived from offset comparison
   * (backward compatible).
   *
   * Returns the window coordinates returned by the server, or `undefined` if
   * the response was discarded (stale sequence / aborted).
   */
  loadSessionMessages: (
    agentId: string,
    sessionId: string,
    offset?: number,
    limit?: number,
    options?: { evictionDirection?: "head" | "tail" | "none" },
  ) => Promise<{ offset: number; limit: number; total: number } | undefined>;
  abortSessionLoad: (agentId: string, sessionId: string) => void;
  /**
   * One-shot jump to the latest page: replace cache with the LAST
   * MESSAGE_CACHE_WINDOW raw entries (offset=0, limit=MESSAGE_CACHE_WINDOW).
   *
   * Used by all "navigate to the bottom" scenarios:
   *  - Initial session mount / session switch
   *  - Scroll-to-bottom button click
   *  - Initial state where the cache is parked at an older window
   *
   */
  ensureLatestInCache: (agentId: string, sessionId: string) => Promise<void>;
  /**
   * One-shot jump to the oldest page: replace cache with the FIRST
   * MESSAGE_CACHE_WINDOW raw entries (offset=messageTotal-limit).
   *
   * Symmetric to ensureLatestInCache. Used by the scroll-to-top button
   * to jump directly to the beginning of the conversation without
   * paginating one page at a time.
   */
  ensureOldestInCache: (agentId: string, sessionId: string) => Promise<void>;
  /**
   * ADR-038: Strict UI-only switch — set which session is in the foreground.
   *
   * Contract: does NOT modify `openSessionIds` and does NOT communicate with
   * the backend. Used when the user toggles between already-open tabs. The
   * caller MUST have already ensured the session is in `openSessionIds`
   * (typically via `openSession`). If the session is not in `openSessionIds`
   * we log a warning and no-op — the caller should be calling `openSession`.
   */
  setActiveTab: (agentId: string, sessionId: string) => void;
  /**
   * Apply session metadata (model/provider/workspace_id) from activate_session response.
   * Sets the session's model/provider and agent's preferredModel, plus syncs workspaceStore.
   */
  applySessionMeta: (agentId: string, sessionId: string, meta: { model?: string | null; provider?: string | null; workspace_id?: string | null }) => void;
  /**
   * Get the active session ID for an agent */
  getActiveSessionId: (agentId: string) => string | null;
  /**
   * ADR-014: Get session state for reading from external stores */
  getSessionState: (agentId: string, sessionId: string) => SessionChatState;
  /**
   * ADR-014: Update session status from backend (Pull repair) */
  updateSessionStatus: (agentId: string, sessionId: string, status: SessionStatus) => void;
  /**
   * ADR-014: Batch update session statuses — single set() call to avoid O(n) re-renders */
  batchUpdateSessionStatuses: (agentId: string, statuses: Map<string, SessionStatus>) => void;
  /**
   * ADR-038: Open a session — full UI + backend activation.
   *
   * Three side effects, all idempotent:
   *  1. UI: add to `openSessionIds` (cap MAX_OPEN_TABS) and set as `activeSessionId`.
   *  2. Backend: send MQTT `open_session` — Runtime transitions Closed/NotFound → Active
   *     and acks via `session_opened` (or errors via `session_not_opened`).
   *  3. Local cache: HTTP-load conversation history for the session.
   *
   * After this returns, the UI shows the session and the user can interact,
   * but `isSessionReady` flips true only when `session_opened` arrives.
   * The input box should remain disabled until that event lands (or fire a
   * `session_not_opened` toast with a reopen affordance).
   */
  openSession: (agentId: string, sessionId: string) => Promise<void>;
  /** ADR-015: Open a session tab (append to openSessionIds).
   *  @deprecated since ADR-038 — use `openSession` (which combines UI +
   *  backend activation). Kept for any caller that only wants the UI half
   *  of `openSession` without sending an MQTT message. */
  openTab: (agentId: string, sessionId: string) => void;
  /** ADR-038: Close a session tab AND notify the backend.
   *
   * Three side effects:
   *  1. UI: remove from `openSessionIds`, activate a neighbor tab.
   *  2. Backend: send MQTT `close_session` so Runtime drops the in-memory
   *     session task. The JSONL + meta stay on disk; reopen is via
   *     `openSession`.
   *
   * Returns the new active sessionId (or null when no tabs remain).
   */
  closeTab: (agentId: string, sessionId: string) => Promise<string | null>;
  /** ADR-015: Get open session IDs for an agent */
  getOpenSessionIds: (agentId: string) => string[];
  /** ADR-032 C4c: Send a user-initiated compression action (tool results or summary).
   *  `compressType` is the proto `CompressType` enum value:
   *    1 = SUMMARY, 2 = TOOL_RESULTS. */
  sendCompressAction: (agentId: string, sessionId: string, compressType: number) => void;
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

// ADR-033 / ADR-036: Initialize MQTT event listeners.
//
// We register TWO separate Tauri-event subscriptions:
//   - `agent-event`:  business message stream (status, chunks, sessions, ...)
//                     routed to `handleMessageEvent`.  This was the only
//                     listener before ADR-036 and is unchanged.
//   - `mqtt-status`:  connection liveness (`{connected: bool, reason?: str}`)
//                     emitted by the Rust eventloop on every CONNACK /
//                     DISCONNECT.  This is the *source-of-truth* path for
//                     `chatStore.mqttConnected`; we DO NOT set the flag in
//                     `initMqttListener` itself any more.
//
// Additionally we synchronously query `get_mqtt_status` after the listeners
// are registered.  That command returns the Rust-side last observed
// status, which lets us recover the initial state without racing the
// Tauri event between `connect_mqtt` returning and `listen` resolving.
//
// Both unlisten handles must be cleaned up together in
// `disposeMqttListener`.
let _mqttAgentEventUnlisten: (() => void) | null = null;
let _mqttStatusUnlisten: (() => void) | null = null;

export async function initMqttListener(): Promise<void> {
  // Unregister previous listeners if any (idempotent re-init).
  if (_mqttAgentEventUnlisten) {
    _mqttAgentEventUnlisten();
    _mqttAgentEventUnlisten = null;
  }
  if (_mqttStatusUnlisten) {
    _mqttStatusUnlisten();
    _mqttStatusUnlisten = null;
  }

  _mqttAgentEventUnlisten = await listen("agent-event", (event) => {
    const data = event.payload as Record<string, unknown>;
    const agentId = data.agent_id as string;
    if (!agentId) {
      // Events without an agent_id are ignored at the store level
      return;
    }

    const store = useChatStore;
    handleMessageEvent(data, store.setState, store.getState, agentId);
  });

  // ADR-036: connection liveness is owned by the Rust eventloop.  We only
  // consume.  The Tauri side always includes `connected` (and optionally
  // `reason` when disconnected); we mirror the value verbatim onto the
  // store.  We DO NOT optimistically set `mqttConnected: true` here —
  // the very first `mqtt-status` event with `connected: true` will set it.
  _mqttStatusUnlisten = await listen<{ connected: boolean; reason?: string }>(
    "mqtt-status",
    (event) => {
      const { connected, reason } = event.payload;
      useChatStore.setState({
        mqttConnected: connected,
        lastMqttError: connected ? null : reason ?? null,
      });
    },
  );

  // Pull the *current* status from the Rust side so we don't miss the
  // initial state.  The poll task has been running since `connect_mqtt`
  // returned, and any transition that happened before this listener
  // registered is reflected in the shared `last_mqtt_status` slot.
  //
  // `known: false` means the poll task hasn't observed any transition
  // yet (e.g. broker is reachable but ConnAck hasn't arrived).  We
  // treat that exactly like "unknown" — neither flip mqttConnected to
  // true nor populate lastMqttError.  The first `mqtt-status` event
  // will fill in the real state.
  try {
    const snapshot = await invoke<{
      known: boolean;
      connected: boolean;
      reason?: string | null;
    }>("get_mqtt_status");
    if (snapshot.known) {
      useChatStore.setState({
        mqttConnected: snapshot.connected,
        lastMqttError: snapshot.connected ? null : snapshot.reason ?? null,
      });
    }
  } catch (err) {
    // Tauri command not registered (older binary) or other transient
    // failure — fall back to the event stream alone.
    console.warn("get_mqtt_status failed; relying on mqtt-status events:", err);
  }
}

export function disposeMqttListener(): void {
  if (_mqttAgentEventUnlisten) {
    _mqttAgentEventUnlisten();
    _mqttAgentEventUnlisten = null;
  }
  if (_mqttStatusUnlisten) {
    _mqttStatusUnlisten();
    _mqttStatusUnlisten = null;
  }
  useChatStore.setState({ mqttConnected: false, lastMqttError: null });
}

export const useChatStore = create<ChatStore>((set, get) => ({
  agentStates: {},
  mqttConnected: false,
  lastMqttError: null,
  availableModels: [],

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

  // ADR-038 @deprecated: Pure UI tab-open without backend activation.
  // Most callers should use `openSession` instead, which sends the MQTT
  // `open_session` ack to the Runtime and hydrates local message cache.
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

  // ADR-038: Close a session tab AND notify the backend.
  //
  // Three side effects (all best-effort — MQTT publish errors are logged
  // but do not block UI cleanup, which is the source of truth for the
  // frontend tab strip):
  //  1. UI: remove from `openSessionIds`, activate a neighbor tab.
  //  2. Backend: send MQTT `close_session` so Runtime drops the in-memory
  //     session task. The JSONL + meta stay on disk; reopen is via
  //     `openSession`.
  //  3. Reset `isSessionReady` on the evicted session so a later switch
  //     back must wait for a fresh `session_opened` ack.
  closeTab: async (agentId: string, sessionId: string): Promise<string | null> => {
    let newActiveId: string | null = null;
    set((state) => {
      const agent = getAgentState(state, agentId);
      const idx = agent.openSessionIds.indexOf(sessionId);
      if (idx === -1) return {}; // Not open — nothing to do

      const newOpenIds = agent.openSessionIds.filter((id) => id !== sessionId);

      // If closing the active tab, activate neighbor
      if (agent.activeSessionId === sessionId) {
        // Prefer right neighbor, then left
        const neighborIdx = Math.min(idx, newOpenIds.length - 1);
        newActiveId = newOpenIds[neighborIdx] ?? null;
      } else {
        newActiveId = agent.activeSessionId;
      }

      // ADR-038: drop readiness for the evicted session so a later switch
      // must wait for a fresh `session_opened` ack from the Runtime.
      const newSessionStates = { ...agent.sessionStates };
      const evictedState = newSessionStates[sessionId];
      if (evictedState) {
        newSessionStates[sessionId] = { ...evictedState, isSessionReady: false };
      }

      return updateAgentState(state, agentId, {
        openSessionIds: newOpenIds,
        activeSessionId: newActiveId,
        sessionStates: newSessionStates,
      });
    });

    // 2. Backend: tell Runtime to release the session task.  `close_session`
    // is idempotent on the Runtime side (Closed → closed no-op), so it is
    // safe to fire even if the backend has already dropped the session.
    try {
      await invoke("mqtt_publish_control", {
        agentId,
        command: "close_session",
        payloadJson: { session_id: sessionId },
      });
    } catch (err) {
      console.warn("[chatStore] close_session MQTT failed (UI already cleaned up):", err);
    }

    return newActiveId;
  },

  // ADR-015: Get open session IDs for reading
  getOpenSessionIds: (agentId: string): string[] => {
    return getAgentState(get(), agentId).openSessionIds;
  },

  /** ADR-032 C4c: Send a user-initiated compression action to the Runtime.
   *  ADR-034 Phase 5: dedicated compress_action command with compress_type. */
  sendCompressAction: (agentId: string, sessionId: string, compressType: number) => {
    invoke("mqtt_publish_control", {
      agentId,
      command: "compress_action",
      payloadJson: { session_id: sessionId, compress_type: compressType },
    }).catch((err: unknown) => console.warn("[ChatStore] compress_action via MQTT failed:", err));
  },

  /**
   * ADR-038: Strict UI-only switch — set which session is in the foreground.
   *
   * Contract: does NOT modify `openSessionIds` and does NOT communicate with
   * the backend. Used when the user toggles between already-open tabs.
   *
   * Behaviour:
   *  - If the session is not in `openSessionIds`, we warn and no-op:
   *    the caller should be using `openSession` for first-open semantics.
   *  - If the session is already `activeSessionId`, no-op (idempotent).
   *  - Otherwise: flip `activeSessionId` to the target session.
   *
   * We do NOT touch per-session state here — `openSession` owns session-state
   * creation, and `session_created` / `session_opened` events refresh fields
   * like `model` / `provider`. `setActiveTab` is purely "which tab is shown".
   */
  setActiveTab: (agentId: string, sessionId: string) => {
    set((state) => {
      const agent = getAgentState(state, agentId);
      if (!agent.openSessionIds.includes(sessionId)) {
        console.warn(
          `[chatStore] setActiveTab: ${sessionId} not in openSessionIds — ` +
          `call openSession(agentId, sessionId) first`,
        );
        return {};
      }
      if (agent.activeSessionId === sessionId) return {};
      return updateAgentState(state, agentId, { activeSessionId: sessionId });
    });
  },

  /**
   * ADR-038: Open a session with full UI + backend activation.
   *
   * Three side effects (idempotent, best-effort):
   *  1. UI: append to `openSessionIds` if not already present (cap MAX_OPEN_TABS),
   *     set as `activeSessionId`, ensure `sessionStates[sessionId]` exists.
   *  2. Backend: send MQTT `open_session`. Runtime will ack via `session_opened`
   *     (flipping `isSessionReady`) or `session_not_opened` (triggering toast).
   *  3. Local cache: HTTP-load conversation history for the session.
   *
   * The MQTT send is fire-and-forget: errors are logged but don't reject the
   * UI transition. If the user just stopped / restarted the agent, MQTT may
   * not be connected yet — the publish fails, but the UI is already correct.
   * The Runtime will publish `session_opened` once the connection is healthy.
   */
  openSession: async (agentId: string, sessionId: string) => {
    // 1. UI: open the tab + activate + ensure session cache slot.
    set((state) => {
      const agent = getAgentState(state, agentId);
      const patches: Partial<AgentState> = { activeSessionId: sessionId };

      if (!agent.openSessionIds.includes(sessionId)) {
        const newOpenIds = [...agent.openSessionIds, sessionId].slice(-MAX_OPEN_TABS);
        patches.openSessionIds = newOpenIds;
      }

      // Lazy-create the session state entry so downstream consumers
      // (loadSessionMessages / fetchSessionState / etc.) don't have to
      // handle a missing entry.
      const newSessionStates = { ...agent.sessionStates };
      if (!newSessionStates[sessionId]) {
        newSessionStates[sessionId] = {
          ...makeInitialSessionState({ ...agent, ...patches }),
          lastAccessed: Date.now(),
        };
      } else {
        newSessionStates[sessionId] = {
          ...newSessionStates[sessionId],
          lastAccessed: Date.now(),
        };
      }
      patches.sessionStates = newSessionStates;

      return updateAgentState(state, agentId, patches);
    });

    // 2. Backend: tell Runtime to transition Closed → Active (or Active no-op).
    // Best-effort — MQTT may be temporarily disconnected; the Runtime will
    // still ack correctly when the broker reconnects.
    try {
      await invoke("mqtt_publish_control", {
        agentId,
        command: "open_session",
        payloadJson: { session_id: sessionId },
      });
    } catch (err) {
      console.warn("[chatStore] open_session MQTT failed:", err);
    }

    // 3. Local cache: load conversation history (initial-load semantics).
    // Failures are non-fatal; the user can retry or wait for incremental
    // MQTT patches.
    try {
      await get().loadSessionMessages(agentId, sessionId);
    } catch (err) {
      console.warn("[chatStore] loadSessionMessages after openSession failed:", err);
    }
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
        messageOffset: 0,
        messageLimit: 0,
        messageTotal: 0,
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
        messageOffset: 0,
        messageLimit: 0,
        messageTotal: 0,
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
      set((state) => {
        const ss = getSessionState(state, agentId, sessionId);
        return updateSessionState(state, agentId, sessionId,
          applyTrimAndAdjustMeta(ss, [...ss.messages, userMsg], true));
      });
      // ── DIAG: verify optimistic user message insert ──
      console.log("[ChatStore:DEBUG] sendMessage optimistic insert", {
        sid: sessionId,
        userMsgId,
        messagesLenAfter: getSessionState(get(), agentId, sessionId).messages.length,
      });
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
        messageOffset: 0,
        messageLimit: 0,
        messageTotal: 0,
      });
    });
  },

  setCurrentModel: (model: string, provider: string, agentId: string) => {
    const sessionId = getAgentState(get(), agentId).activeSessionId;
    console.log("[ChatStore:DEBUG] setCurrentModel called", { model, provider, agentId, sessionId });
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
  resolveQuestion: (agentId: string, requestId: string) => {
    const sessionId = getAgentState(get(), agentId).activeSessionId;
    if (!sessionId) return;
    set((state) => updateSessionState(state, agentId, sessionId, {
      pendingQuestions: (getSessionState(state, agentId, sessionId).pendingQuestions ?? []).filter(
        (q) => q.request_id !== requestId
      ),
    }));
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
    offset?: number,
    limit: number = 50,
    options?: { evictionDirection?: "head" | "tail" | "none" },
  ): Promise<{ offset: number; limit: number; total: number } | undefined> => {
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

    // "Initial load" = the caller is fetching the session for the first time
    // (caller passes `offset === undefined`) AND the cache is empty.  This is
    // the only path that:
    //   - shows the "Loading conversation..." indicator
    //   - clears streaming buffers (activeStream entries that predate this
    //     load cycle and would otherwise be orphaned)
    //
    // offset===0 with a non-empty cache is NOT initial — it's a user-initiated
    // "jump to the latest page" (ensureLatestInCache from a middle-of-conversation
    // cache position).  We must NOT clear streaming in that case (the agent may
    // still be writing live), and we must NOT show the loading overlay (the
    // cache already has content to render).
    const cacheIsEmpty = sessionState.messages.length === 0;
    const isInitialLoad = offset === undefined && cacheIsEmpty;
    if (isInitialLoad) {
      set((state) => ({
        ...updateSessionState(state, agentId, sessionId, { isLoadingSession: true, loadError: null }),
      }));
    }

    try {
      const params = new URLSearchParams();
      params.set("limit", String(limit));
      params.set("offset", String(offset ?? 0));
      // ADR-035 Phase 3: no HTTP incremental endpoint

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

      const converted = mergeDocumentUploads(data.messages ?? [], agentId);
      const returnedOffset = data.offset;
      const returnedLimit = data.limit;
      const returnedTotal = data.total;

      set((state) => {
        const ss = getSessionState(state, agentId, sessionId);
        const prevOffset = ss.messageOffset;
        const prevLimit = ss.messageLimit;

        let nextMessages: ChatMessage[];

        // ADR-041 C2: evictionDirection controls cache window trimming.
        // - 'none': no trimming (window grows) - used by adapter loadBefore/loadAfter
        // - 'tail': drop newest (slide window toward past)
        // - 'head': drop oldest (slide window toward present)
        // - undefined: derive from offset comparison (backward compatible)
        const eviction = options?.evictionDirection;
        const derivedDirection: "head" | "tail" =
          returnedOffset > prevOffset ? "tail" : "head";
        const effectiveDirection = eviction ?? derivedDirection;

        // When evictionDirection='none', we need to track the full merged
        // offset range so hasOlder/hasNewer are computed correctly.
        // messageOffset = min(returnedOffset, prevOffset) = newest offset in cache
        // messageLimit = max(returnedOffset+returnedLimit, prevOffset+prevLimit) - messageOffset
        let finalOffset = returnedOffset;
        let finalLimit = returnedLimit;

        if (isInitialLoad) {
          clearSessionStreaming(sessionId);
          nextMessages = trimOldest(converted);
        } else if (returnedOffset > prevOffset) {
          // Loading OLDER messages (scroll-up). Prepend older messages.
          const existingIds = new Set(ss.messages.map((m) => m.id));
          const older = converted.filter((m) => !existingIds.has(m.id));
          const merged = [...older, ...ss.messages];
          if (effectiveDirection === "none") {
            nextMessages = merged;
            // Compute the full merged range: offset 0 = newest, higher = older.
            // prevOffset/prevLimit describe the old cache; returnedOffset/returnedLimit
            // describe the newly loaded older page.
            finalOffset = Math.min(returnedOffset, prevOffset);
            finalLimit = Math.max(
              returnedOffset + returnedLimit,
              prevOffset + (ss.messages.length > 0 ? prevLimit : 0),
            ) - finalOffset;
          } else {
            nextMessages = merged.length > MESSAGE_CACHE_WINDOW
              ? merged.slice(0, MESSAGE_CACHE_WINDOW)
              : merged;
          }
        } else if (returnedOffset < prevOffset) {
          // Loading NEWER messages (scroll-down). Append newer messages.
          const existingIds = new Set(ss.messages.map((m) => m.id));
          const newer = converted.filter((m) => !existingIds.has(m.id));
          const merged = [...ss.messages, ...newer];
          if (effectiveDirection === "none") {
            nextMessages = merged;
            finalOffset = Math.min(returnedOffset, prevOffset);
            finalLimit = Math.max(
              returnedOffset + returnedLimit,
              prevOffset + (ss.messages.length > 0 ? prevLimit : 0),
            ) - finalOffset;
          } else {
            nextMessages = merged.length > MESSAGE_CACHE_WINDOW
              ? merged.slice(merged.length - MESSAGE_CACHE_WINDOW)
              : merged;
          }
        } else {
          const existingIds = new Set(ss.messages.map((m) => m.id));
          const same = converted.filter((m) => !existingIds.has(m.id));
          nextMessages = [...ss.messages, ...same];
        }

        return updateSessionState(state, agentId, sessionId, {
          messages: nextMessages,
          messageOffset: finalOffset,
          messageLimit: finalLimit,
          messageTotal: returnedTotal,
          isLoadingSession: false,
          loadError: null,
        });
      });

      return { offset: returnedOffset, limit: returnedLimit, total: returnedTotal };
    } catch (e: unknown) {
      if (getSessionState(get(), agentId, sessionId).loadSequence !== seq) {
        console.log(`[ChatStore] Discarding stale error response (seq ${seq})`);
        return;
      }
      if (e instanceof DOMException && e.name === "AbortError") {
        console.log(`[ChatStore] loadSessionMessages aborted (seq ${seq})`);
        set((state) => updateSessionState(state, agentId, sessionId, { isLoadingSession: false, isLoadingMore: false }));
        return;
      }
      console.error("[ChatStore] Failed to load session messages:", e);
      set((state) => updateSessionState(state, agentId, sessionId, {
        messages: [],
        messageOffset: 0,
        messageLimit: 0,
        messageTotal: 0,
        isLoadingSession: false,
        loadError: `${i18n.t("chatPanel.sessionLoadFailed")}: ${e instanceof Error ? e.message : String(e)}`,
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


  /**
   * One-shot jump to the latest page (offset=0, limit=MESSAGE_CACHE_WINDOW).
   *
   * Replaces the old `loadMoreNewerMessages` loop which had to issue N HTTP
   * requests to slide a cache from the middle to the tail.  After this call
   * the cache holds the LAST `MESSAGE_CACHE_WINDOW` raw entries (or all of
   * them if the session has fewer); `messageOffset` becomes 0; the rendering
   * layer's ensureRenderable effect then decides if more prepended data is
   * needed to fill the viewport (same load-older path).
   *
   * No-op if the cache is already at the latest page (`messageOffset === 0`)
   * — the caller is expected to check `messageOffset > 0` before invoking.
   */
  ensureLatestInCache: async (agentId: string, sessionId: string) => {
    const sessionState = getSessionState(get(), agentId, sessionId);
    if (sessionState.isLoadingMore) return;
    const { messageOffset, messageTotal, messages } = sessionState;
    // Already at the newest page:
    //   - messageOffset === 0 (window anchored at the tail), AND
    //   - messageTotal > 0 (we know the conversation has data — guards against
    //     a freshly-initialized sessionState whose DEFAULT values are all 0 /
    //     empty, which would otherwise be mistaken for "already at tail"), AND
    //   - messages.length covers at least min(WINDOW, total) items (the cache
    //     window is fully populated at the tail end).
    //
    // The naive `if (messageOffset === 0) return;` check incorrectly no-ops
    // on first-ever load of a new session: DEFAULT_SESSION_STATE initializes
    // messageOffset to 0, so the very call that is supposed to load the first
    // page short-circuits and leaves the user staring at a blank chat.
    const tailCovered =
      messageOffset === 0 &&
      messageTotal > 0 &&
      messages.length >= Math.min(MESSAGE_CACHE_WINDOW, messageTotal);
    if (tailCovered) return;
    set((state) => updateSessionState(state, agentId, sessionId, { isLoadingMore: true }));
    try {
      await get().loadSessionMessages(agentId, sessionId, 0, MESSAGE_CACHE_WINDOW);
    } finally {
      set((state) => updateSessionState(state, agentId, sessionId, { isLoadingMore: false }));
    }
  },

  ensureOldestInCache: async (agentId: string, sessionId: string) => {
    const sessionState = getSessionState(get(), agentId, sessionId);
    if (sessionState.isLoadingMore) return;
    const { messageOffset, messageLimit, messageTotal, messages } = sessionState;
    // Already at the oldest page?
    //   - The cache window's far edge touches the end of the conversation
    //     (messageOffset + messageLimit >= messageTotal), AND
    //   - We have data (messageTotal > 0).
    const headCovered =
      messageTotal > 0 &&
      messageOffset + messageLimit >= messageTotal &&
      messages.length >= Math.min(MESSAGE_CACHE_WINDOW, messageTotal);
    if (headCovered) return;
    set((state) => updateSessionState(state, agentId, sessionId, { isLoadingMore: true }));
    try {
      // Load the oldest page: offset = max(0, total - WINDOW).
      // offset=0 means newest; higher offset = older.
      // The oldest page starts at offset = total - limit (clamped to 0).
      const limit = MESSAGE_CACHE_WINDOW;
      const oldestOffset = Math.max(0, messageTotal - limit);
      await get().loadSessionMessages(agentId, sessionId, oldestOffset, limit);
    } finally {
      set((state) => updateSessionState(state, agentId, sessionId, { isLoadingMore: false }));
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
  // Maps the /api/agents/{id}/sessions/{sid} response to SessionChatState fields.
  //
  // ADR-039: The Runtime returns `{ session_id, meta, live_state }` where:
  //   - `meta` = authoritative per-session config from data/meta/{session_id}.json
  //     (model, provider, workspace_id, reasoning_effort, temperature, tokens, etc.)
  //   - `live_state` = runtime snapshot (status, ratio, todos, context_usage)
  //     or `null` when no live session is running.
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
        meta: Record<string, unknown>;
        live_state: Record<string, unknown> | null;
      };
      const { meta, live_state: liveState } = data;
      if (!meta && !liveState) return;

      const sessionPatch: Partial<SessionChatState> = {};

      // ── From meta.json (authoritative persistent fields) ──
      if (meta) {
        if (typeof meta.model === "string" && meta.model) sessionPatch.model = meta.model as string;
        if (typeof meta.provider === "string" && meta.provider) sessionPatch.provider = meta.provider as string;
        if (typeof meta.reasoning_effort === "string" && meta.reasoning_effort) {
          sessionPatch.reasoningEffort = meta.reasoning_effort as string;
        }
        // NaN is the Runtime "no override" sentinel for temperature.
        if (typeof meta.temperature === "number" && !Number.isNaN(meta.temperature)) {
          sessionPatch.temperature = meta.temperature as number;
        }
        if (typeof meta.message_count === "number") {
          sessionPatch.messageTotal = meta.message_count as number;
        }
      }

      // ── From live_state (runtime fields) ──
      if (liveState) {
        // Status: parse from JSON
        if (liveState.status && typeof liveState.status === "object") {
          sessionPatch.sessionStatus = liveState.status as SessionStatus;
        }
        // Model chars/token ratio from API calibration.
        if (typeof liveState.ratio === "number") sessionPatch.ratio = liveState.ratio as number;
        // Todo list
        if (liveState.todos && Array.isArray(liveState.todos)) {
          sessionPatch.todos = liveState.todos as TodoItem[];
        }
        // Context usage (token counts, window, percentage)
        if (liveState.context_usage && typeof liveState.context_usage === "object") {
          sessionPatch.contextUsage = liveState.context_usage as ContextUsageInfo;
        }
      }

      if (Object.keys(sessionPatch).length > 0) {
        set((state) => updateSessionState(state, agentId, sessionId, sessionPatch));
      }
      // Workspace selection is owned by workspaceStore, not SessionChatState.
      if (meta && typeof meta.workspace_id === "string" && meta.workspace_id) {
        useWorkspaceStore.getState().setSessionWorkspaceLocal(sessionId, meta.workspace_id as string);
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
    // ADR-035: tool_call_id is the backend's authoritative pairing key
    // (see ExploreBlock.buildPairedItems). It MUST be loaded from JSONL so
    // that historical sessions pair tool_call ↔ tool_result correctly even
    // before any live MQTT events arrive.
    base.toolCallId = meta.tool_call_id as string | undefined;
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
  "reasoning_started", "reasoning_ended",
  "memory_updated", "skill_executed",
  "session_meta", "session_config",
  "stream_delta", "record_complete",
]);

// ── ADR-035: upsert a message into a session's messages[] by id ──
//
// Real-time MQTT appends (record_complete / tool_call / etc.) slide the
// cache window forward: trim the oldest entries from the front.
function upsertMessageInSession(state: ChatStore, agentId: string, sid: string, msg: ChatMessage): Partial<ChatStore> {
  const ss = getSessionState(state, agentId, sid);
  const idx = ss.messages.findIndex(m => m.id === msg.id);
  if (idx >= 0) {
    const arr = ss.messages.slice();
    arr[idx] = msg;
    return updateSessionState(state, agentId, sid, applyTrimAndAdjustMeta(ss, arr, false));
  }
  return updateSessionState(state, agentId, sid, applyTrimAndAdjustMeta(ss, [...ss.messages, msg], true));
}

// ── Per-session seq-ordered insert (root fix for reorder) ──
//
// Live entries (`stream_delta` / `record_complete`) carry a per-session
// monotonic `seq` produced by the Runtime's `next_seq()` counter at the
// chunk_relay (single-threaded). The Desktop MUST respect that order when
// placing entries in `messages[]`, otherwise an out-of-order delivery
// from the broker would shove a fresh round's streaming placeholder into
// the middle of the previous round's tool_call / tool_result block,
// breaking the tool_call ↔ tool_result pairing in `ExploreBlock`.
//
// Algorithm:
//   1. If `msg.seq` is `undefined`, fall back to `upsertMessageInSession`
//      — covers history-loaded JSONL entries (which never carry `seq`).
//   2. Else, look up an existing entry by id (placeholder ↔ freeze share
//      a message_id; replacing freezes the placeholder) and if found,
//      update in place but PRESERVE `seq` from the EXISTING entry.  This
//      keeps a streaming record's frozen seq equal to the first
//      stream_delta placeholder seq, which is what the seq sort order
//      expects.
//   3. Else, binary-search the first index whose `seq > msg.seq` and
//      insert there. History entries (no seq) sort after all live
//      entries because `undefined ?? -Infinity` is smaller than any
//      real seq — wait, that's wrong: messages[] already has live
//      entries appended (we sort live ones among themselves) and any
//      JSONL entry loaded later has undefined seq, which should sit at
//      the tail (newest in user view). Per-id replace handles JSONL
//      late-arrival; new JSONL entries (which lack seq) deliberately
//      keep the legacy append-to-end semantics for backward compat.
function insertBySeq(state: ChatStore, agentId: string, sid: string, msg: ChatMessage): Partial<ChatStore> {
  const ss = getSessionState(state, agentId, sid);
  // Backward compat: history / pre-seq Runtime → legacy append/replace
  if (msg.seq == null) {
    return upsertMessageInSession(state, agentId, sid, msg);
  }
  const arr = ss.messages.slice();
  // Same id → freeze / update in place (placeholder ↔ record_complete
  // share the runtime-assigned message_id; tool_call / tool_result
  // records don't have a placeholder but a duplicate id is still safe).
  const byIdIdx = arr.findIndex(m => m.id === msg.id);
  if (byIdIdx >= 0) {
    const existing = arr[byIdIdx];
    // Preserve existing seq on the in-place update: stream_delta
    // pushes use the same seq as the placeholder; record_complete
    // carries that same seq back, so the array sort order is invariant.
    arr[byIdIdx] = { ...existing, ...msg, seq: existing.seq ?? msg.seq };
    return updateSessionState(state, agentId, sid, applyTrimAndAdjustMeta(ss, arr, false));
  }
  // New entry: binary search the insertion point by seq ascending.
  // Live entries are strictly increasing in seq, so binary search is
  // O(log n).
  //
  // History entries (seq undefined) are loaded via HTTP from on-disk
  // JSONL. They were persisted BEFORE the current session started, so
  // they are always OLDER than any live entry. Using `-Infinity` for
  // undefined seq ensures they sort BEFORE all live entries, which
  // matches the chronological timeline: history (oldest, lower index)
  // → live (newest, higher index).
  let lo = 0;
  let hi = arr.length;
  while (lo < hi) {
    const mid = (lo + hi) >>> 1;
    // History entries have no seq → treat as infinitely small so they
    // sit before (to the left of) any seq-carrying live entry.
    const midSeq = arr[mid].seq ?? Number.NEGATIVE_INFINITY;
    if (midSeq <= msg.seq) {
      lo = mid + 1;
    } else {
      hi = mid;
    }
  }
  arr.splice(lo, 0, msg);
  return updateSessionState(state, agentId, sid, applyTrimAndAdjustMeta(ss, arr, true));
}

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

    // ADR-035: new_data_available removed. Push-driven streaming below.
    case "stream_delta": {
      if (!sid) break;
      const lines = (data.lines as Array<{role:string;message_id:string;line_no:number;content:string}>) ?? [];
      // ── DIAG: log every incoming stream_delta ──
      console.log("[ChatStore:DEBUG] stream_delta RECEIVED", {
        sid,
        eventSessionId: data.session_id,
        msgId: lines[0]?.message_id,
        role: lines[0]?.role,
        lineCount: lines.length,
        seq: data.seq,
        activeStreamMessageId: activeStreams.get(sid)?.messageId,
        messagesLenBefore: getSessionState(get(), agentId, sid).messages.length,
      });
      if (!lines.length) break;
      const role = lines[0].role === 'assistant' ? 'assistant' as const : 'thought' as const;
      const msgId = lines[0].message_id;
      // Per-session monotonic seq from the Runtime's chunk_relay. Carried
      // through into the placeholder so record_complete (which also carries
      // the same seq) lands at the same position on freeze, and so the
      // binary-search insert in `insertBySeq` slots this placeholder at
      // the correct position in `messages[]` even under broker reorder.
      const incomingSeq = typeof data.seq === 'number' ? (data.seq as number) : undefined;
      let as = activeStreams.get(sid);
      const prevMsgId = as?.messageId;
      if (!as || as.messageId !== msgId) {
        // Record the physical predecessor: the last message currently in
        // messages[] at the time this stream starts. Conversation data is
        // linearly ordered — this relationship is immutable. When
        // record_complete arrives, checking whether prevMessageId is still
        // in the cache window determines whether to freeze or discard.
        // (Kept as a legacy safety net; under seq ordering the freeze is
        // anchored by id + seq anyway.)
        const state = get();
        const msgs = getSessionState(state, agentId, sid!).messages;
        const prevMessageId = msgs.length > 0 ? msgs[msgs.length - 1].id : null;
        as = { messageId: msgId, role, lines: [], prevMessageId, seq: incomingSeq };
        activeStreams.set(sid, as);
        set(state => insertBySeq(state, agentId, sid!, {
          id: msgId,
          type: role,
          content: '',
          isStreaming: true,
          startTime: Date.now(),
          timestamp: Date.now(),
          ...(incomingSeq != null ? { seq: incomingSeq } : {}),
          ...getAgentSenderInfo(agentId),
        }));
        // ── DIAG: verify placeholder actually inserted ──
        console.log("[ChatStore:DEBUG] stream_delta PLACEHOLDER CREATED", {
          sid,
          msgId,
          role,
          seq: incomingSeq,
          messagesLenAfter: getSessionState(get(), agentId, sid!).messages.length,
        });
        if (prevMsgId) notifyActiveStreamSubscribers(sid, prevMsgId);
      }
      if (!as) break;
      for (const l of lines) as.lines.push({ role: l.role === 'assistant' ? 'assistant' : 'thought', lineNo: l.line_no, content: l.content });
      if (as.role === 'thought' && as.lines.length > 5) {
        as.lines = as.lines.slice(-5);
      } else if (as.role === 'assistant' && as.lines.length > ASSISTANT_LINE_SAFETY_CAP) {
        // ADR-035 C2 safety valve: assistant content is never truncated for
        // display, but if record_complete is lost AND idle realignment fails,
        // prevent unbounded memory growth. Keep the newest lines. This should
        // never trigger in normal operation — if it does, the record_complete
        // delivery path has a bug.
        console.warn(
          `[ChatStore] ADR-035 C2 safety valve: assistant activeStream hit` +
          ` ${ASSISTANT_LINE_SAFETY_CAP}-line cap (messageId=${as.messageId}).` +
          ` record_complete likely lost — trimming oldest to prevent OOM.`,
        );
        as.lines = as.lines.slice(-ASSISTANT_LINE_SAFETY_CAP);
      }
      // Rebuild the cached snapshot with a fresh reference BEFORE notifying
      // subscribers, so the next useSyncExternalStore getSnapshot() returns
      // a different object than the previous render and the bubble actually
      // re-renders with the new content.  Without this, line changes land
      // in `as.lines` but `getStreamingContent` would still return the
      // pre-rebuild snapshot → React thinks nothing changed → no re-render.
      streamingSnapshots.set(sid, {
        content: as.lines.map(l => l.content).join("\n"),
        isStreaming: true,
        lineCount: as.lines.length,
      });
      // Flip the per-session `isAssistantReplying` flag once the line count
      // crosses the threshold. Done OUTSIDE the per-message bubble render
      // path so the indicator is a stable session-level signal that cannot
      // be left stuck "on" after record_complete (cleared in that handler).
      // Edge-triggered: only update when the boolean actually changes,
      // otherwise every stream_delta would re-render the entire ChatPanel.
      if (as.role === 'assistant') {
        const shouldBeReplying = as.lines.length > ASSISTANT_REPLYING_LINE_THRESHOLD;
        const isCurrentlyReplying = getSessionState(get(), agentId, sid).isAssistantReplying;
        if (shouldBeReplying !== isCurrentlyReplying) {
          set((state) => updateSessionState(state, agentId, sid, {
            isAssistantReplying: shouldBeReplying,
          }));
        }
      }
      notifyActiveStreamSubscribers(sid, msgId);
      break;
    }

    case "record_complete": {
      if (!sid) break;
      // ── DIAG: log every incoming record_complete ──
      console.log("[ChatStore:DEBUG] record_complete RECEIVED", {
        sid,
        eventSessionId: data.session_id,
        msgId: data.message_id,
        role: data.role,
        seq: data.seq,
        contentLen: typeof data.content === 'string' ? data.content.length : 0,
        activeStreamMessageId: activeStreams.get(sid)?.messageId,
        messagesLenBefore: getSessionState(get(), agentId, sid).messages.length,
      });
      const rawRole = data.role as string;
      const role = (rawRole === 'assistant' || rawRole === 'thought' || rawRole === 'tool_call' || rawRole === 'tool_result')
        ? rawRole as 'assistant' | 'thought' | 'tool_call' | 'tool_result'
        : 'assistant';
      const msgId = data.message_id as string;
      const payloadContent = (data.content as string) ?? '';
      // Per-session seq from the Runtime; matches the seq used by the
      // matching stream_delta placeholder. Required for `insertBySeq` to
      // land the freeze at the right slot, and lets direct tool_call /
      // tool_result records (no streaming phase) slot in alongside their
      // sibling entries.
      const incomingSeq = typeof data.seq === 'number' ? (data.seq as number) : undefined;
      // ADR-035: backend now forwards tool metadata in record_complete for
      // tool_call / tool_result records. We pull it here so ExploreBlock's
      // buildPairedItems can match tool_call ↔ tool_result by toolCallId
      // and render the right tool label without an HTTP refresh.
      const toolName = (data.tool_name as string | undefined) ?? '';
      const toolCallId = (data.tool_call_id as string | undefined) ?? '';
      const isError = data.is_error === true;
      const toolStatus: "success" | "error" = isError ? "error" : "success";
      // The terminal event for any streaming assistant message has arrived
      // (or this record is for a non-streamed role like tool_call).  Either
      // way, the standalone "replying" indicator is no longer warranted —
      // clear it unconditionally so the flag can never stay stuck "on".
      // Edge-triggered: skip the set if it's already false (idempotent
      // record_complete for tool_call/tool_result without a prior stream).
      if (getSessionState(get(), agentId, sid).isAssistantReplying) {
        set((state) => updateSessionState(state, agentId, sid, {
          isAssistantReplying: false,
        }));
      }
      const as = activeStreams.get(sid);
      if (as && as.messageId === msgId) {
        // Stream completed. Check whether the physical predecessor
        // (prevMessageId) is still in the messages[] cache window.
        // Conversation data is linearly ordered — if the predecessor is
        // in the window, this record is continuous and must be frozen in.
        // If the predecessor was evicted by scroll-back trim, the user
        // scrolled away → discard; HTTP will load this record (which sits
        // right after its predecessor) when the user scrolls back.
        activeStreams.delete(sid);
        const state = get();
        const msgs = getSessionState(state, agentId, sid).messages;
        const prevInWindow = as.prevMessageId == null
          || msgs.some(m => m.id === as.prevMessageId);
        if (prevInWindow) {
          const fc = role === 'thought' ? as.lines.map(l => l.content).join('\n') : payloadContent;
          // `insertBySeq` replaces the placeholder in-place (same id) and
          // preserves the original seq (via the by-id branch). This keeps
          // the frozen record at the same position the placeholder
          // occupied, invariant across the stream_delta → record_complete
          // pair.
          set(s => insertBySeq(s, agentId, sid, {
            id: msgId,
            type: role,
            content: fc,
            isStreaming: false,
            endTime: Date.now(),
            timestamp: Date.now(),
            ...(incomingSeq != null ? { seq: incomingSeq } : {}),
            ...getAgentSenderInfo(agentId),
          }));
        }
        // else: predecessor evicted → user scrolled away → discard.
        // HTTP scroll-back will load the complete record from JSONL.
      } else {
        // tool_call / tool_result (no activeStream) — insert via seq.
        // Populate tool metadata so downstream pairing/rendering can work.
        const extraFields: Partial<ChatMessage> = {};
        if (role === 'tool_call' || role === 'tool_result') {
          if (toolName) extraFields.toolName = toolName;
          if (toolCallId) extraFields.toolCallId = toolCallId;
          extraFields.toolStatus = toolStatus;
          // tool_result content is JSON-stringified; surface it as toolData
          // so the bubble renderer can pretty-print it.
          if (role === 'tool_result') {
            try {
              const parsed = JSON.parse(payloadContent);
              if (parsed && typeof parsed === 'object') {
                extraFields.toolData = parsed as Record<string, unknown>;
              }
            } catch {
              // Not JSON — keep raw in `content`, do not set toolData.
            }
          }
        }
        set(state => insertBySeq(state, agentId, sid!, {
          id: msgId,
          type: role,
          content: payloadContent,
          isStreaming: false,
          endTime: Date.now(),
          timestamp: Date.now(),
          ...(incomingSeq != null ? { seq: incomingSeq } : {}),
          ...getAgentSenderInfo(agentId),
          ...extraFields,
        }));
      }
      notifyActiveStreamSubscribers(sid, msgId);
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

    case "reasoning_started": {
      if (!sid) break;
      set((state) => updateSessionState(state, agentId, sid!, { isReasoning: true }));
      break;
    }

    case "reasoning_ended": {
      if (!sid) break;
      set((state) => updateSessionState(state, agentId, sid!, { isReasoning: false }));
      break;
    }

    case "error": {
      if (!sid) break;
      // Backend sends user_message as content, plus detail and error_type
      const errorMsg = (data.content ?? data.message) as string;
      const errorDetail = (data.detail) as string | undefined;
      const errorType = (data.error_type) as string | undefined;
      console.error("[ChatStore] Server error:", errorMsg, errorDetail);
      // ADR-035 Phase 3: no polling
      const errMsg: ChatMessage = {
        id: `msg-error-${Date.now()}`,
        type: "error",
        content: errorMsg as string,
        errorDetail: errorDetail || undefined,
        errorType: errorType || undefined,
        timestamp: Date.now(),
        ...getAgentSenderInfo(agentId),
      };
      set((state) => {
        const ss = getSessionState(state, agentId, sid!);
        return {
          ...updateSessionState(state, agentId, sid!, {
            ...applyTrimAndAdjustMeta(ss, [...ss.messages, errMsg], true),
            isCompacting: false,
          }),
        };
      });
      break;
    }

    case "stopped": {
      if (!sid) break;
      // ADR-035 Phase 3: no polling
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
        set((state) => {
          const current = getSessionState(state, agentId, sid).pendingQuestions ?? [];
          // Avoid duplicates if the same request_id arrives twice (MQTT QoS)
          if (current.some((q) => q.request_id === (data as unknown as AskQuestionEvent).request_id)) {
            return state;
          }
          return updateSessionState(state, agentId, sid, {
          pendingQuestions: [...current, data as unknown as AskQuestionEvent],
          });
        });
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
        get().loadSessionMessages(agentId, sid);  // ADR-035 Phase 3: no incremental
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

            // ADR-039: persistent fields (model, provider, reasoning_effort,
            // temperature, workspace_id) are delivered through the
            // `session_meta` MQTT channel, not here. Only runtime fields
            // (ratio, context_usage) are included in this event.
            // Model chars/token ratio from API calibration (for status panel display).
            if (typeof data.ratio === "number") sessionPatch.ratio = data.ratio as number;
            // ADR-028: Context usage snapshot from persisted session tokens.
            // The backend includes this on session activation/resume so the
            // frontend can show token counts before the first LLM call
            // triggers a dedicated context_usage event.
            if (data.context_usage && typeof data.context_usage === "object") {
              sessionPatch.contextUsage = data.context_usage as ContextUsageInfo;
            }

            // ADR-021: Start/stop polling based on status transitions.
            // When entering streaming/waiting_approval/paused → start polling.
            // ADR-035 Phase 3: prevActive/nextActive removed — push drives streaming.
            const prev = getSessionState(state, agentId, sid);

            // ADR-035 Phase 3: streaming driven by push; no HTTP polling on status transitions.

            // When status transitions TO Idle from non-Idle, clear pending flags
            if (prev.sessionStatus?.status !== "idle" && status.status === "idle") {
              sessionPatch.pendingApproval = {};
              sessionPatch.pendingQuestions = [];
              sessionPatch.iterationLimitPaused = null;
              sessionPatch.isAssistantReplying = false;

              // ADR-035 C2/O2: if activeStream still has unfrozen content at
              // idle, record_complete was lost (QoS edge case). Trigger HTTP
              // full realignment to recover — this is the only HTTP pull-back
              // scenario, fires only when push delivery is suspected incomplete.
              // assistant content is NEVER truncated; we recover the full
              // message from JSONL via the HTTP reload.
              const activeStream = activeStreams.get(sid);
              if (activeStream) {
                console.warn(
                  `[ChatStore] ADR-035 C2: activeStream still present at idle` +
                  ` (messageId=${activeStream.messageId}, role=${activeStream.role},` +
                  ` lines=${activeStream.lines.length}) — record_complete likely lost,` +
                  ` triggering HTTP realignment`,
                );
                activeStreams.delete(sid);
                // Defer the HTTP reload to after the state update completes.
                queueMicrotask(() => {
                  get().loadSessionMessages(agentId, sid);
                });
              }
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

            // Update session state (status) then agent-level defaults
            const sessionResult = updateSessionState(state, agentId, sid, sessionPatch);
            let agentStates = sessionResult.agentStates;

            // ADR-039: model, provider, workspace_id are delivered through
            // the `session_meta` MQTT channel, not here.
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
      // ADR-038: a freshly-created session is Active by definition (the
      // backend has just spawned the session task), so we use strict
      // `setActiveTab` (no openSessionIds mutation, no MQTT round-trip)
      // to flip the foreground tab.  Any caller that initiated the
      // creation must have already called `createSession`, which itself
      // routes through the same MQ-backed lifecycle — so the session is
      // guaranteed to be in `openSessionIds` and Active by the time
      // this event lands.
      agentStore.activateNewlyCreatedSession(newSessionId, agentId);
      break;
    }

    // ADR-038: Runtime ack that an OpenSession command succeeded.
    // Topic: acowork/agents/{id}/sessions/{sid}/opened (Retained).
    // Flips `isSessionReady` so the UI unlocks the input box and
    // applies the runtime's authoritative model/provider/last_active_at
    // (defensive — applySessionMeta already covers most of this from
    // session_state_changed events, but having both paths makes the
    // contract observable in the store alone).
    case "session_opened": {
      const sid = data.session_id as string | undefined;
      if (!sid) break;
      const status = data.status as string | undefined;
      const model = typeof data.model === "string" && data.model ? data.model : null;
      const provider = typeof data.provider === "string" && data.provider ? data.provider : null;
      const lastActiveAt =
        typeof data.last_active_at === "string" && data.last_active_at ? data.last_active_at : null;
      console.log(
        `[ChatStore] session_opened for ${agentId}/${sid}: status=${status ?? "?"}, ` +
        `model=${model ?? "?"}, provider=${provider ?? "?"}, last_active_at=${lastActiveAt ?? "?"}`,
      );
      set((state) => {
        const agent = getAgentState(state, agentId);
        const newSessionStates = { ...agent.sessionStates };
        const existing = newSessionStates[sid] ?? {
          ...makeInitialSessionState(agent),
          lastAccessed: Date.now(),
        };
        newSessionStates[sid] = {
          ...existing,
          isSessionReady: true,
          ...(model ? { model } : {}),
          ...(provider ? { provider } : {}),
          lastAccessed: Date.now(),
        };
        return updateAgentState(state, agentId, { sessionStates: newSessionStates });
      });
      break;
    }

    // ADR-038: Runtime error when the session is not Active.
    // Topic: acowork/agents/{id}/sessions/{sid}/not_opened.
    // We surface this to the UI as a warning toast with a one-click
    // "reopen" affordance so the user can recover from the rare case
    // where the frontend issued chat_message / model_switch without a
    // preceding open_session (e.g. an older client, or a bug elsewhere
    // in the lifecycle). The backend stays the source of truth — when
    // the contract is violated, we make it visible.
    case "session_not_opened": {
      const sid = data.session_id as string | undefined;
      if (!sid) break;
      const reason = (data.reason as string | undefined) ?? "session_closed";
      const attempted = (data.attempted_command as string | undefined) ?? "?";
      console.warn(
        `[ChatStore] session_not_opened for ${agentId}/${sid}: ` +
        `attempted=${attempted}, reason=${reason}`,
      );
      set((state) => {
        const agent = getAgentState(state, agentId);
        const existing = agent.sessionStates[sid];
        if (!existing) return {};
        return updateSessionState(state, agentId, sid, {
          isSessionReady: false,
        });
      });
      // Only toast if this is the active session — background tabs
      // hitting a state error don't deserve a modal callout.
      const activeSid = getAgentState(get(), agentId).activeSessionId;
      if (activeSid === sid) {
        showToast({
          type: "warning",
          message: `Session is not open (${reason}). Reopen it to continue.`,
          action: {
            label: "Reopen",
            onClick: () => {
              void useChatStore.getState().openSession(agentId, sid);
            },
          },
        });
      }
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

    // ── Session meta (model_id, provider_id, tokens, title, message_count) ──
    case "session_meta": {
      console.log("[ChatStore:DEBUG] session_meta RECEIVED", {
        sid,
        agentId,
        model_id: data.model_id,
        provider_id: data.provider_id,
        workspace_id: data.workspace_id,
        reasoning_effort: data.reasoning_effort,
        temperature: data.temperature,
        title: data.title,
        hasSessionId: !!data.session_id,
      });
      if (!sid) break;
      // Title goes into the agent-level sessions[] list (used by the
      // sidebar) so it shows up the moment the retained MQTT message arrives,
      // not only after a manual fetchSessions.
      if (typeof data.title === "string" && data.title) {
        useAgentStore.getState().updateSessionTitle(sid, data.title);
      }
      const patch: Partial<SessionChatState> = {};
      if (typeof data.model_id === "string" && data.model_id) patch.model = data.model_id;
      if (typeof data.provider_id === "string" && data.provider_id) patch.provider = data.provider_id;
      if (typeof data.message_count === "number") patch.messageTotal = data.message_count;
      if (typeof data.reasoning_effort === "string" && data.reasoning_effort) {
        patch.reasoningEffort = data.reasoning_effort;
      }
      // NaN is the Runtime "no override" sentinel for temperature.
      if (typeof data.temperature === "number" && !Number.isNaN(data.temperature)) {
        patch.temperature = data.temperature;
      }
      if (Object.keys(patch).length > 0) {
        console.log("[ChatStore:DEBUG] session_meta applying patch", { sid, patch });
        set((state) => updateSessionState(state, agentId, sid!, patch));
      }
      // Workspace selection is owned by workspaceStore, not SessionChatState.
      if (typeof data.workspace_id === "string" && data.workspace_id) {
        console.log("[ChatStore:DEBUG] session_meta setting workspace", { sid, workspace_id: data.workspace_id });
        useWorkspaceStore.getState().setSessionWorkspaceLocal(sid, data.workspace_id);
      }
      break;
    }

    // ── Agent lifecycle: status, meta, config ──
    case "agent_status": {
      const aid = data.agent_id as string | undefined;
      const online = data.online as boolean | undefined;
      if (aid && online !== undefined) {
        useAgentStore.getState().updateAgentOnlineStatus(aid, online);
      }
      break;
    }

    case "agent_meta": {
      const aid = data.agent_id as string | undefined;
      if (aid) {
        useAgentStore.getState().patchAgentMeta(aid, {
          name: data.name as string | undefined,
          version: data.version as string | undefined,
          avatar: data.avatar as string | undefined,
          builtin_avatar: data.builtin_avatar as string | undefined,
        });
      }
      break;
    }

    case "agent_config": {
      const aid = data.agent_id as string | undefined;
      if (aid && typeof data.config_json === "string") {
        try {
          const config = JSON.parse(data.config_json);
          console.log("[ChatStore] Agent config updated:", aid, config);
          // ADR-034 §7.6.4: Runtime republishes its merged AgentConfig
          // (manifest defaults + agent_config.json) as a retained MQTT
          // message on startup and after each PUT /api/agents/{id}/config.
          // The desktop Setup / Tools panels listen to
          // `acowork:refresh-agent-config` to re-fetch via the HTTP
          // reverse proxy. Without this emit, a Runtime-initiated
          // change (e.g. workspace_switch side effects) would silently
          // leave the Setup panel showing stale localStorage values.
          emitAgentConfigRefresh(aid);
        } catch (e) {
          console.warn("[ChatStore] Failed to parse agent_config JSON:", e);
        }
      }
      break;
    }

    case "sidecar_status": {
      // Sidecar status is currently a debug feature without agent_id routing.
      // The listener at line 609 filters out events without agent_id, so this
      // branch is reached only if agent_id is added to the Rust forwarding.
      console.log("[ChatStore] Sidecar status:", {
        kind: data.kind,
        endpoint: data.endpoint,
        ready: data.ready,
      });
      break;
    }

    case "session_config": {
      if (sid && typeof data.config_json === "string") {
        console.log("[ChatStore] Session config updated:", sid, data.config_json.slice(0, 120));
      }
      break;
    }

    case "memory_node_update": {
      console.log("[ChatStore] Memory node update:", {
        node_id: data.node_id,
        agent_id: data.agent_id,
      });
      break;
    }

    default:
      console.log("[ChatStore] Unknown event type:", eventType, data);
  }
}
