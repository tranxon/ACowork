import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { ChatMessage, ContextUsageInfo, TokenUsage, ToolApprovalNeededEvent, PaginatedMessages, ConversationEntry, SessionStatus, AskQuestionEvent, ModelEntry, TodoItem, ActiveStream, AttachedItem } from "../lib/types";
import { toWireAttachedItems } from "../lib/types";
import { useAgentStore } from "./agentStore";
import { useGatewayStore } from "./gatewayStore";
import { useUserProfileStore } from "./userProfileStore";
import { useWorkspaceStore } from "./workspaceStore";
import { getGatewayUrl } from "../lib/config";
import { emitAgentConfigRefresh } from "../lib/refresh";
import i18n from "../i18n";
import { showToast } from "../components/common/ToastProvider";
import { log } from "../lib/logger";

// ---------------------------------------------------------------------------
// ADR-035 convergent model: per-session active stream tracker.
// Keyed by sessionId.  Used solely for the isAssistantReplying indicator
// (lineCount threshold).  No real-time content rendering.
// ---------------------------------------------------------------------------

// ActiveStream type lives in lib/types.ts.
const activeStreams = new Map<string, ActiveStream>();

// Throttle timestamp for thinking content flush (per session).
// Limits Zustand set() calls to at most every 500ms during thought streaming.
const lastThinkingFlush = new Map<string, number>();
// Throttle timestamp for assistant streaming content flush (per session).
// Same 500ms cadence as `lastThinkingFlush`. Both share the same intent:
// stream_delta can arrive every ~50-200ms during long generations, but
// the trailing preview only needs to update at human-perceptible cadence
// (500ms = ~2Hz) to look "live" without piling up Zustand re-renders.
const lastAssistantFlush = new Map<string, number>();

// Debounce timers for HTTP refresh triggered by record_complete.
// Keyed by `${agentId}:${sessionId}`.  Multiple record_complete events
// arriving within the debounce window are coalesced into a single HTTP
// request, reducing redundant fetches during rapid tool-call sequences.
const refreshTimers = new Map<string, ReturnType<typeof setTimeout>>();

/**
 * Schedule a debounced HTTP refresh of the session's message window.
 *
 * Called from the `record_complete` handler and from `setPinnedToBottom`
 * (catch-up when the user returns to the bottom).
  *
 * Guards:
 *  - isPinnedToBottom (early exit): user is not at the bottom – skip
 *    entirely.  Prevents message array mutations (append/replace) from
 *    causing re-renders while the user is scrolled up.  When the user
 *    returns, `setPinnedToBottom(true)` re-triggers this function.
 *  - isLoadingMore: don't interfere with an in-flight user-initiated
 *    pagination request (loadBefore / loadAfter share the same
 *    per-session AbortController).
 *
 * NOTE: The former `messageOffset !== 0` guard was removed because it
 * blocked the very refresh that resets messageOffset to 0.  When the
 * user scrolls back to bottom (isPinnedToBottom becomes true) while
 * messageOffset is still > 0, the HTTP fetch at offset=0 is what
 * brings the window back to the newest page.  isPinnedToBottom alone
 * is sufficient to decide whether a refresh should fire.
 */
function scheduleRefresh(agentId: string, sessionId: string): void {
  // Early exit: if the user is not at the bottom, don't even set a timer.
  // The catch-up is handled by setPinnedToBottom(true) -> scheduleRefresh.
  {
    const ss = getSessionState(useChatStore.getState(), agentId, sessionId);
    if (!ss.isPinnedToBottom) return;
  }

  const key = `${agentId}:${sessionId}`;
  const prev = refreshTimers.get(key);
  if (prev) clearTimeout(prev);
  refreshTimers.set(key, setTimeout(() => {
    refreshTimers.delete(key);
    // useChatStore is defined later in this module; safe to reference
    // here because this callback runs asynchronously after the module
    // has fully loaded.
    const store = useChatStore.getState();
    const ss = getSessionState(store, agentId, sessionId);
    if (ss.isLoadingMore) return;
    if (!ss.isPinnedToBottom) return;  // re-check after debounce
    // No messageTotal === 0 guard: record_complete itself proves data
    // exists in the Runtime's storage.  An HTTP fetch with offset=0 will
    // return the latest records regardless of the stale local total.
    void store.loadSessionMessages(agentId, sessionId, 0, 50);
  }, 200));
}



// ── messages[] cache: grows continuously, no sliding window ──
//
// The messages array grows as the user scrolls up (loadBefore prepends)
// or as new messages arrive via streaming (append).  No trimming is
// performed — the cache holds all messages that have been loaded.
// Memory is released when the session is closed or switched away.
// At ~593 messages / 3 MB, this is well within acceptable limits.
//
// Counted in raw-entry units (same as backend `PaginatedMessages.limit`).
// A single display group can occupy multiple slots here, which is fine:
// the cache only bounds memory, the UI rendering handles folding.

/**
 * Append a new message to the cache and adjust pagination metadata.
 *
 * **Sliding-window invariant**: `messages` is ALWAYS a contiguous window
 * into the full conversation.  When `messageOffset === 0` the window
 * touches the newest entry, so appending a real-time message keeps it
 * contiguous.  When `messageOffset > 0` the user has scrolled up – the
 * new message (at offset 0) is NOT adjacent to the window, so we must
 * NOT append it.  The message will be loaded via `scheduleRefresh` when
 * the user returns to the bottom.
 */
function appendMessageAndAdjustMeta(
  ss: SessionChatState,
  newMessages: ChatMessage[],
): Partial<SessionChatState> {
  if (ss.messageOffset === 0) {
    // At the bottom – append directly, no trimming.
    return {
      messages: [...ss.messages, ...newMessages],
      messageTotal: ss.messageTotal + 1,
      messageLimit: ss.messageLimit + 1,
    };
  }
  // Not at the bottom – don't touch messages array.  Just bump the
  // metadata so hasNewer / hasOlder stay accurate.
  return {
    messageTotal: ss.messageTotal + 1,
    messageOffset: ss.messageOffset + 1,
  };
}

// ── ADR-035 C2: assistant activeStream safety valve ──
//
// assistant content is NEVER truncated for display — the user must see the
// full reply. But if record_complete is lost (QoS edge case) AND the idle
// realignment fallback also fails (e.g. session closed mid-stream), the
// activeStream.lineCount would grow unbounded and leak memory.
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
  /** Loop detected pause — populated from loop_detected_paused event */
  loopDetectedPaused: { message: string } | null;
  /** 429 retry wait info — populated from session_state when the provider is rate-limited */
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
   * so the indicator is independent
   * of any single MessageBubble's render lifecycle and can never get stuck
   * "on" after record_complete.
   */
  isAssistantReplying: boolean;
  /** True while the agent is in the thinking/reasoning phase. */
  isThinking: boolean;
  /** Timestamp when the current thinking phase started. */
  thinkingStartTime: number | null;
  /** Latest thought lines (cap 5, joined) for ThinkBlock preview. */
  thinkingContent: string;
  /**
   * Latest assistant text lines (joined) for the trailing streaming
   * preview rendered as a `StreamingSourceBlock variant="assistant"`
   * virtual item in VirtualMessageList. Mirrors the `thinkingContent`
   * pattern: live preview during `stream_delta`, cleared on
   * `record_complete`. Capped at the source (last 5 lines from the
   * store) to keep DOM memory flat during long streams.
   */
  assistantStreamingContent: string;
  /** Timestamp when the current assistant stream started (used for
   *  duration display in StreamingSourceBlock). Mirrors
   *  `thinkingStartTime`. Reset on each new assistant messageId. */
  assistantStreamingStartTime: number | null;
  /**
   * Whether the user is pinned to the bottom of the scrollable area.
   * Updated by useScrollController's state machine via `setPinnedToBottom`.
   *
   * Single source of truth for "is user at the bottom?" – used by:
   *  - scheduleRefresh: skip HTTP message refresh when not at bottom
   *  - thinkingContent flush: skip preview update when not at bottom
   *  - Arrow button: derived (show when !isPinnedToBottom)
   *
   * This is a SCROLL-POSITION flag (viewport level), distinct from
   * `messageOffset` which is a PAGINATION flag (cache-window level).
   */
  isPinnedToBottom: boolean;
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
  /** ADR-045: Per-tool progress heartbeat state.
   *  Keyed by tool_call_id. Entry is created on first `tool_progress`
   *  event (5s after tool start) and removed when the matching
   *  `tool_result` record arrives. Absence of an entry means the tool
   *  completed in <5s — keep the pre-ADR-045 UX (breathing dot). */
  toolProgress: Record<string, { elapsedMs: number; timeoutMs: number }>;
  /**
   * Server-side error from MQTT `error` event.  NOT persisted to JSONL
   * by the backend, so it must NOT enter the `messages` array (which is
   * a sliding window over JSONL).  Rendered as a dismissible banner in
   * ChatPanel, same pattern as RetryWaitBanner / DebugPausedBanner.
   */
  serverError: { content: string; errorDetail?: string; errorType?: string; timestamp: number } | null;
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
  loopDetectedPaused: null,
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
  isThinking: false,
  thinkingStartTime: null,
  thinkingContent: '',
  assistantStreamingContent: '',
  assistantStreamingStartTime: null,
  isPinnedToBottom: true,
  isReasoning: false,
  isSessionReady: false,
  isLoadingMore: false,
  toolProgress: {} as Record<string, { elapsedMs: number; timeoutMs: number }>,
  serverError: null,
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
    // Release the evicted session's activeStream tracker.
    activeStreams.delete(id);
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
  sendMessage: (content: string, agentId: string, command?: string, attachedItems?: AttachedItem[]) => Promise<void>;
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
   * No sliding window — the messages array grows continuously and is released
   * on closeSession or session switch.
   *
   * Returns the window coordinates returned by the server, or `undefined` if
   * the response was discarded (stale sequence / aborted).
   */
  loadSessionMessages: (
    agentId: string,
    sessionId: string,
    offset?: number,
    limit?: number,
  ) => Promise<{ offset: number; limit: number; total: number } | undefined>;
  abortSessionLoad: (agentId: string, sessionId: string) => void;
  /**
   * One-shot jump to the latest page (offset=0).
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
   * raw entries (offset=messageTotal-limit).
   *
   * Symmetric to ensureLatestInCache. Used by the scroll-to-top button
   * to jump directly to the beginning of the conversation without
   * paginating one page at a time.
   */
  ensureOldestInCache: (agentId: string, sessionId: string) => Promise<void>;
  /**
   * Release the messages array for a session to free memory.
   * Called on closeSession and session switch.  The session state
   * is preserved (offset/limit/total) so the metadata can be used
   * to decide whether to reload from scratch or from a specific page.
   */
  clearSessionMessages: (agentId: string, sessionId: string) => void;
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
  /** ADR-045: Cancel an in-flight tool execution by tool_call_id.
   *  Publishes a `cancel_tool` control message to the Runtime.
   *  The Runtime maps toolCallId → pending tokio task and aborts it. */
  cancelTool: (agentId: string, sessionId: string, toolCallId: string) => void;
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
  /** Remove a single attachment entry from the in-memory message list.
   *
   *  Each ADR-046 attachment (file_upload / image_upload / attached_*)
   *  is rendered as a standalone system entry with its own `message.id`.
   *  Calling this with that id drops the entry from `messages[]`.
   *
   *  MVP scope: in-memory only. The corresponding JSONL line is NOT
   *  removed — a session reload will bring the attachment back, and
   *  runtime token accounting already reflects the attached item.
   *  Bidirectional sync (delete from JSONL + tell the runtime) is
   *  deferred to a follow-up; see ADR-046 follow-ups. */
  removeMessageAttachment: (agentId: string, sessionId: string, messageId: string) => void;
  /** Remove a message from the per-session queue by index */
  removeQueuedMessage: (agentId: string, sessionId: string, index: number) => void;
  /** Replace the entire per-session queue (e.g. after sending all) */
  setQueuedMessages: (agentId: string, sessionId: string, messages: string[]) => void;
  /**
   * Update the scroll-position flag `isPinnedToBottom`.
   *
   * Called by useScrollController's `transitionTo` when the state machine
   * enters/leaves "pinned-bottom".  When transitioning back to bottom
   * (false -> true), a catch-up `scheduleRefresh` is triggered so that
   * any messages that arrived while the user was scrolled up are loaded.
   */
  setPinnedToBottom: (agentId: string, sessionId: string, value: boolean) => void;
  /** Dismiss the server-side error banner. */
  clearServerError: (agentId: string, sessionId: string) => void;
  /** ADR-047: Pull session state (status/ratio/todos/context_usage) from backend.
   *  Config fields (model/provider/workspace_id/reasoning_effort/temperature)
   *  are NOT applied here - use `fetchSessionConfig` or `loadSession`. */
  fetchSessionState: (agentId: string, sessionId: string) => Promise<void>;
  /** ADR-047: Pull session config (model/provider/workspace_id/reasoning_effort/
   *  temperature/title) from backend `GET /sessions/{sid}/config`. */
  fetchSessionConfig: (agentId: string, sessionId: string) => Promise<void>;
  /** ADR-047 §3.5.2: Combined cold-load function. Must be used in all
   *  switch/open/first-start scenarios. Encapsulates `Promise.all` of
   *  `fetchSessionState` + `fetchSessionConfig` so callers cannot
   *  accidentally skip one. */
  loadSession: (agentId: string, sessionId: string) => Promise<void>;
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

/// Reentrancy guard for `initMqttListener`.
///
/// React StrictMode (dev) double-invokes `useEffect`, causing
/// `bootGateway` → `initMqttListener` to be called twice concurrently.
/// Without this guard, the second call cancels the first call's
/// listeners before they finish registering, causing `mqtt-status`
/// events to be lost.  The guard ensures the second call awaits the
/// first and returns immediately.
let _mqttInitPromise: Promise<void> | null = null;

/// Interval handle for the background status-polling fallback.
///
/// When `get_mqtt_status` returns `connected: false` (e.g. the client
/// is still in `Connecting` state after a wake-recovery reload), the
/// `mqtt-status` event with `connected: true` may have been emitted
/// *before* the listener was registered and was therefore lost.  The
/// polling fallback calls `get_mqtt_status` every 1 s until the client
/// reports `connected: true` or 30 attempts have been made.  This is
/// purely a safety net - the event listener is the primary mechanism.
let _mqttPollHandle: ReturnType<typeof setInterval> | null = null;

function stopMqttPoll(): void {
  if (_mqttPollHandle) {
    clearInterval(_mqttPollHandle);
    _mqttPollHandle = null;
  }
}

function startMqttPoll(): void {
  stopMqttPoll();
  let attempts = 0;
  _mqttPollHandle = setInterval(async () => {
    attempts++;
    if (attempts > 30) {
      stopMqttPoll();
      log.debug("MQTT status polling exhausted 30 attempts; relying on events only");
      return;
    }
    try {
      const snap = await invoke<{ known: boolean; connected: boolean; reason?: string | null }>(
        "get_mqtt_status",
      );
      if (attempts <= 3 || attempts % 10 === 0) {
        log.debug("[mqtt-poll] attempt", attempts, "snapshot:", JSON.stringify(snap));
      }
      if (snap.known && snap.connected) {
        useChatStore.setState({ mqttConnected: true, lastMqttError: null });
        stopMqttPoll();
        log.debug("[mqtt-poll] connected confirmed after", attempts, "poll(s)");
      }
    } catch {
      // Transient IPC failure - keep polling.
    }
  }, 1000);
}

export async function initMqttListener(): Promise<void> {
  // Reentrancy guard: if a previous init is still in flight (React
  // StrictMode double-call), await it and return instead of racing.
  if (_mqttInitPromise) {
    await _mqttInitPromise;
    return;
  }
  _mqttInitPromise = doInitMqttListener();
  try {
    await _mqttInitPromise;
  } finally {
    _mqttInitPromise = null;
  }
}

async function doInitMqttListener(): Promise<void> {
  // Unregister previous listeners and stop any ongoing poll.
  stopMqttPoll();
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

  // ADR-036: connection liveness is owned by the Rust eventloop.  We
  // consume `mqtt-status` events for real-time updates.  The payload
  // may include `connecting: true` or `reconnecting: true` for
  // transient states that should NOT trigger the disconnected banner.
  _mqttStatusUnlisten = await listen<{
    connected: boolean;
    reason?: string;
    connecting?: boolean;
    reconnecting?: boolean;
  }>("mqtt-status", (event) => {
    const { connected, reason, connecting, reconnecting } = event.payload;
    if (connected) {
      useChatStore.setState({ mqttConnected: true, lastMqttError: null });
      stopMqttPoll();
    } else if (connecting) {
      // Client is attempting to connect - don't flash the error banner.
      useChatStore.setState({ mqttConnected: false, lastMqttError: null });
    } else if (reconnecting) {
      // Client lost connection and is retrying - show a warning.
      useChatStore.setState({ mqttConnected: false, lastMqttError: reason ?? "reconnecting" });
    } else {
      // Hard disconnect with a reason.
      useChatStore.setState({ mqttConnected: false, lastMqttError: reason ?? null });
    }
  });

  // Pull the *current* status from the Rust side so we don't miss the
  // initial state.  The source of truth is `DesktopMqttClient::session_state`
  // (a watch channel updated synchronously by the poll task).
  //
  // If the snapshot shows `connected: false` (client is in `Connecting`
  // or `Reconnecting` state), start the polling fallback to catch the
  // eventual `Connected` transition - the `mqtt-status` event for that
  // transition may have been emitted before this listener registered.
    try {
      const snapshot = await invoke<{
        known: boolean;
        connected: boolean;
        reason?: string | null;
      }>("get_mqtt_status");
      log.debug("[initMqttListener] snapshot:", snapshot);
      if (snapshot.known) {
      useChatStore.setState({
        mqttConnected: snapshot.connected,
        lastMqttError: snapshot.connected ? null : snapshot.reason ?? null,
      });
    }
    // Start the polling fallback whenever we are not yet connected.
    // It is harmless when the event stream is working and is a
    // lifeline when the initial event was lost during webview reload.
    if (!snapshot.connected) {
      startMqttPoll();
    }
  } catch (err) {
    // Tauri command not registered (older binary) or other transient
    // failure - fall back to the event stream alone.
    log.warn("get_mqtt_status failed; relying on mqtt-status events:", err);
  }
}

export function disposeMqttListener(): void {
  stopMqttPoll();
  _mqttInitPromise = null;
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
  //  2. Memory: clear the session's messages array (free memory).
  //  3. Backend: send MQTT `close_session` so Runtime drops the in-memory
  //     session task. The JSONL + meta stay on disk; reopen is via
  //     `openSession`.
  //  4. Reset `isSessionReady` on the evicted session so a later switch
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

    // 2. Memory: clear the session's messages array to free memory.
    //    The session state metadata (offset/limit/total) is preserved
    //    so the next load can resume from the correct page.
    get().clearSessionMessages(agentId, sessionId);

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
      log.warn("[chatStore] close_session MQTT failed (UI already cleaned up):", err);
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
    }).catch((err: unknown) => log.warn("[ChatStore] compress_action via MQTT failed:", err));
  },

  /**
   * ADR-045: Cancel a single in-flight tool execution by tool_call_id.
   *
   * The Runtime owns the tool-dispatch task map; the UI only emits the intent.
   * The Runtime looks up the task by toolCallId and aborts it; the resulting
   * tool_result is published as normal (with `cancelled: true` flag if the
   * backend wants to surface it). The UI does NOT optimistically remove the
   * tool_progress entry — we wait for the matching tool_result so the chip
   * transitions through the normal lifecycle (running → cancelled → done).
   */
  cancelTool: (agentId: string, sessionId: string, toolCallId: string) => {
    invoke("mqtt_publish_control", {
      agentId,
      command: "cancel_tool",
      payloadJson: { session_id: sessionId, tool_call_id: toolCallId },
    }).catch((err: unknown) => log.warn("[ChatStore] cancel_tool via MQTT failed:", err));
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
        log.warn(
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
      log.warn("[chatStore] open_session MQTT failed:", err);
    }

    // 3. Local cache: load conversation history + session config/state.
    // ADR-047 §3.5.2: openSession MUST fetch config + state alongside
    // messages so every caller gets a complete session snapshot without
    // relying on React useEffect to trigger loadSession indirectly.
    // Failures are non-fatal; the user can retry or wait for MQTT patches.
    try {
      await get().loadSessionMessages(agentId, sessionId);
    } catch (err) {
      log.warn("[chatStore] loadSessionMessages after openSession failed:", err);
    }
    try {
      await get().loadSession(agentId, sessionId);
    } catch (err) {
      log.warn("[chatStore] loadSession after openSession failed:", err);
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
        loopDetectedPaused: null,
        pendingApproval: {},
        loadError: null,
        hasMoreIncremental: false,
        abortController: null,
        loadSequence: 0,
        serverError: null,
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
        loopDetectedPaused: null,
        pendingApproval: {},
        loadError: null,
        hasMoreIncremental: false,
        abortController: null,
        loadSequence: 0,
        serverError: null,
      }),
    }));
  },

  removeSessionState: (agentId: string, sessionId: string) => {
    activeStreams.delete(sessionId);
    set((state) => {
      const agent = getAgentState(state, agentId);
      const newSessionStates = { ...agent.sessionStates };
      delete newSessionStates[sessionId];
      return updateAgentState(state, agentId, { sessionStates: newSessionStates });
    });
  },

  // ADR-033: connectStream removed — MQTT connection is managed by the Rust backend.
  // The frontend no longer creates WebSocket connections.

  sendMessage: async (content: string, agentId: string, command?: string, attachedItems?: AttachedItem[]) => {
    const sessionId = getAgentState(get(), agentId).activeSessionId;

    // Add user message to the active session's state
    // NOTE: Use crypto.randomUUID() so the ID survives round-trip to the backend
    // and back — loadSessionMessages() deduplicates by message ID, so the
    // optimistic render and the backend-persisted message must share the same ID.
    const userMsgId = `msg-${crypto.randomUUID()}`;

    // ADR-046: every attachment — uploaded documents, uploaded images, AND
    // workspace references — flows through the same `AttachedItem[]` list.
    // The optimistic user message carries only the text content. The
    // attachments are written as separate system entries by the backend
    // and rendered as individual `AttachmentChipRow` in the message list.
    const items = attachedItems ?? [];

    const userMsg: ChatMessage = {
      id: userMsgId,
      type: "user",
      content,
      timestamp: Date.now(),
      ...getUserSenderInfo(),
    };

    if (sessionId) {
      set((state) => {
        const ss = getSessionState(state, agentId, sessionId);
        return updateSessionState(state, agentId, sessionId,
          appendMessageAndAdjustMeta(ss, [...ss.messages, userMsg]));
      });
      // ── DIAG: verify optimistic user message insert ──
      log.debug("[ChatStore:DEBUG] sendMessage optimistic insert", {
        sid: sessionId,
        userMsgId,
        attachedItemCount: items.length,
        messagesLenAfter: getSessionState(get(), agentId, sessionId).messages.length,
      });
    }

    // ADR-046: workspace refs (the `attachedContext` state field, which is
    // also the source of `AttachedContextChips`) are bridged into the same
    // `attached_items` envelope. After this snapshot the field is cleared
    // so the next send starts with a fresh slate. Upload payloads from the
    // caller (already in `items`) come first so upload chips win any
    // filename collisions on render.
    if (sessionId) {
      const ss = getSessionState(get(), agentId, sessionId);
      if (ss.attachedContext.length > 0) {
        for (const ctx of ss.attachedContext) {
          if (ctx.type === "file") {
            items.push({
              type: "attached_file",
              absPath: ctx.absPath,
              name: ctx.name,
            });
          } else if (ctx.type === "selection") {
            items.push({
              type: "attached_selection",
              absPath: ctx.absPath,
              name: ctx.name,
              startLine: ctx.startLine ?? 1,
              endLine: ctx.endLine ?? ctx.startLine ?? 1,
            });
          } else if (ctx.type === "directory") {
            items.push({
              type: "attached_folder",
              absPath: ctx.absPath,
              name: ctx.name,
            });
          }
        }
        set((s) =>
          updateSessionState(s, agentId, sessionId, { attachedContext: [] }),
        );
      }
    }

    // ADR-046 §params: only `attached_items` survives. The legacy
    // `content_parts` / `attached_context` / `document_ids` MQTT params are
    // dropped — the runtime reads everything it needs from
    // `params_json.attached_items`.
    const params: Record<string, unknown> = {};
    if (items.length > 0) params.attached_items = toWireAttachedItems(items);
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
      log.debug("[ChatStore] Message sent via MQTT:", userMsgId);
    } catch (error) {
      log.error("[ChatStore] MQTT message send failed:", error);
      // Transient client-side error - show a toast, don't pollute the
      // messages array (sliding window over JSONL).
      showToast({
        type: "error",
        message: "Failed to send message: Agent may not be connected yet. Please wait and try again.",
      });
    }
  },

  stopCurrentMessage: async (agentId: string) => {
    log.debug("[ChatStore] Stopping current message for agent:", agentId);

    // ADR-034 Phase 5: Send stop via MQTT with reason
    const sessionId = getAgentState(get(), agentId).activeSessionId;
    invoke("mqtt_publish_control", {
      agentId,
      command: "stop",
      payloadJson: { session_id: sessionId, reason: "user_requested" },
    }).catch((err: unknown) => log.warn("[ChatStore] stop via MQTT failed:", err));

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
    }).catch((err: unknown) => log.warn("[ChatStore] sendStop via MQTT failed:", err));

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
    log.debug("[ChatStore:DEBUG] setCurrentModel called", { model, provider, agentId, sessionId });
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
    }).catch((err: unknown) => log.warn("[ChatStore] model_switch via MQTT failed:", err));
  },

  setSessionWorkspaceMqtt: (agentId: string, sessionId: string, workspaceId: string) => {
    invoke("mqtt_publish_control", {
      agentId,
      command: "workspace_switch",
      payloadJson: { workspace_id: workspaceId, session_id: sessionId },
    }).catch((err: unknown) => log.warn("[ChatStore] workspace_switch via MQTT failed:", err));
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
    }).catch((err: unknown) => log.warn("[ChatStore] reasoning_effort via MQTT failed:", err));
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
          ...updateSessionState(state, agentId, sessionId, {
            iterationLimitPaused: null,
            loopDetectedPaused: null,
          }),
        }));
      }
    } catch (error) {
      log.error("[ChatStore] Failed to send continue signal:", error);
    }
  },

  publishUpdateSessionTitle: (agentId: string, sessionId: string, title: string) => {
    invoke("mqtt_publish_control", {
      agentId,
      command: "update_session_title",
      payloadJson: { session_id: sessionId, title },
    }).catch((err: unknown) => log.warn("[ChatStore] update_session_title via MQTT failed:", err));
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
      log.error("[ChatStore] Failed to load conversation history:", e);
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
        log.debug(`[ChatStore] Discarding stale loadSessionMessages response (seq ${seq})`);
        return;
      }

      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);

      const data = (await resp.json()) as PaginatedMessages;

      if (getSessionState(get(), agentId, sessionId).loadSequence !== seq) {
        log.debug(`[ChatStore] Discarding stale response after json parse (seq ${seq})`);
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

        // No sliding window — always merge without trimming.
        // The messages array grows as the user scrolls up or as new
        // messages arrive, and is released on closeSession/switch.
        let finalOffset = returnedOffset;
        let finalLimit = returnedLimit;

        if (isInitialLoad) {
          activeStreams.delete(sessionId);
          nextMessages = converted;
        } else if (returnedOffset > prevOffset) {
          // Loading OLDER messages (scroll-up). Prepend older messages.
          const existingIds = new Set(ss.messages.map((m) => m.id));
          const older = converted.filter((m) => !existingIds.has(m.id));
          nextMessages = [...older, ...ss.messages];
          finalOffset = Math.min(returnedOffset, prevOffset);
          finalLimit = Math.max(
            returnedOffset + returnedLimit,
            prevOffset + (ss.messages.length > 0 ? prevLimit : 0),
          ) - finalOffset;
        } else if (returnedOffset < prevOffset) {
          // Loading NEWER messages (scroll-down). Append newer messages.
          const existingIds = new Set(ss.messages.map((m) => m.id));
          const newer = converted.filter((m) => !existingIds.has(m.id));
          nextMessages = [...ss.messages, ...newer];
          finalOffset = Math.min(returnedOffset, prevOffset);
          finalLimit = Math.max(
            returnedOffset + returnedLimit,
            prevOffset + (ss.messages.length > 0 ? prevLimit : 0),
          ) - finalOffset;
        } else {
          // returnedOffset === prevOffset: same window (e.g. scheduleRefresh
          // or duplicate load).  Merge any newly appeared messages.
          const existingIds = new Set(ss.messages.map((m) => m.id));
          const same = converted.filter((m) => !existingIds.has(m.id));
          nextMessages = [...ss.messages, ...same];
          finalOffset = Math.min(returnedOffset, prevOffset);
          finalLimit = Math.max(
            returnedOffset + returnedLimit,
            prevOffset + (ss.messages.length > 0 ? prevLimit : 0),
          ) - finalOffset;
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
        log.debug(`[ChatStore] Discarding stale error response (seq ${seq})`);
        return;
      }
      if (e instanceof DOMException && e.name === "AbortError") {
        log.debug(`[ChatStore] loadSessionMessages aborted (seq ${seq})`);
        set((state) => updateSessionState(state, agentId, sessionId, { isLoadingSession: false, isLoadingMore: false }));
        return;
      }
      log.error("[ChatStore] Failed to load session messages:", e);
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
   * One-shot jump to the latest page (offset=0).
   *
   * Replaces the old `loadMoreNewerMessages` loop which had to issue N HTTP
   * requests to slide a cache from the middle to the tail.  After this call
   * the cache holds the newest page; `messageOffset` becomes 0; the rendering
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
    //   - messages.length > 0 (the cache has at least some data at the tail).
    const tailCovered =
      messageOffset === 0 &&
      messageTotal > 0 &&
      messages.length > 0;
    if (tailCovered) return;
    set((state) => updateSessionState(state, agentId, sessionId, { isLoadingMore: true }));
    try {
      await get().loadSessionMessages(agentId, sessionId, 0, 50);
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
      messages.length > 0;
    if (headCovered) return;
    set((state) => updateSessionState(state, agentId, sessionId, { isLoadingMore: true }));
    try {
      // Load the oldest page: offset = max(0, total - limit).
      // offset=0 means newest; higher offset = older.
      // The oldest page starts at offset = total - limit (clamped to 0).
      const limit = 50;
      const oldestOffset = Math.max(0, messageTotal - limit);
      await get().loadSessionMessages(agentId, sessionId, oldestOffset, limit);
    } finally {
      set((state) => updateSessionState(state, agentId, sessionId, { isLoadingMore: false }));
    }
  },

  /** Release the messages array for a session to free memory. */
  clearSessionMessages: (agentId: string, sessionId: string) => {
    set((state) => updateSessionState(state, agentId, sessionId, {
      messages: [],
      messageOffset: 0,
      messageLimit: 0,
      messageTotal: 0,
    }));
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

  removeMessageAttachment: (agentId: string, sessionId: string, messageId: string) => {
    set((state) => {
      const ss = getSessionState(state, agentId, sessionId);
      return updateSessionState(state, agentId, sessionId, {
        messages: ss.messages.filter((m) => m.id !== messageId),
      });
    });
  },

  setQueuedMessages: (agentId: string, sessionId: string, messages: string[]) => {
    set((state) => updateSessionState(state, agentId, sessionId, { queuedMessages: messages }));
  },

  setPinnedToBottom: (agentId: string, sessionId: string, value: boolean) => {
    const ss = getSessionState(get(), agentId, sessionId);
    if (ss.isPinnedToBottom === value) return;
    set((state) => updateSessionState(state, agentId, sessionId, { isPinnedToBottom: value }));
    // Catch-up: user returned to the bottom.  Trigger scheduleRefresh
    // so any record_complete events that were blocked while the user
    // was scrolled up are now loaded.  scheduleRefresh will re-check
    // isPinnedToBottom (now true) before firing.
    if (value) {
      scheduleRefresh(agentId, sessionId);
    }
  },

  clearServerError: (agentId: string, sessionId: string) => {
    set((state) => updateSessionState(state, agentId, sessionId, { serverError: null }));
  },

  // ADR-047: Pull session state (runtime telemetry only) from backend.
  //
  // The Runtime returns `{ session_id, meta, live_state }` where:
  //   - `meta` = state-level metadata (created_at, last_active_at, message_count)
  //   - `live_state` = runtime snapshot (status, ratio, todos, context_usage)
  //     or `null` when no live session is running.
  //
  // ADR-047: config fields (model, provider, workspace_id, reasoning_effort,
  // temperature, title) are NO LONGER included in this response. Use
  // `fetchSessionConfig` or `loadSession` to get config.
  // Errors are non-fatal - warns and returns without blocking startup.
  fetchSessionState: async (agentId: string, sessionId: string) => {
    try {
      const resp = await fetch(
        `${getGatewayUrl()}/api/agents/${agentId}/sessions/${sessionId}`,
      );
      if (!resp.ok) {
        log.warn(`[ChatStore] fetchSessionState HTTP ${resp.status} for session ${sessionId}`);
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

      // From meta (state-level metadata only)
      if (meta) {
        if (typeof meta.message_count === "number") {
          sessionPatch.messageTotal = meta.message_count as number;
        }
      }

      // From live_state (runtime fields)
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
    } catch (e) {
      log.warn("[ChatStore] fetchSessionState failed:", e);
    }
  },

  // ADR-047: Pull session config from backend GET /sessions/{sid}/config.
  //
  // The Runtime returns a SessionConfigSnapshot JSON:
  //   { model, provider, workspace_id, reasoning_effort, temperature, title }
  // All fields are optional (null means "not set / no override").
  //
  // This is the authoritative config source - it reads from the in-memory
  // Arc<ConversationSession> which is mutated by apply_config().
  // fetchSessionState does NOT apply config fields; this function does.
  fetchSessionConfig: async (agentId: string, sessionId: string) => {
    try {
      const resp = await fetch(
        `${getGatewayUrl()}/api/agents/${agentId}/sessions/${sessionId}/config`,
      );
      if (!resp.ok) {
        log.warn(`[ChatStore] fetchSessionConfig HTTP ${resp.status} for session ${sessionId}`);
        return;
      }
      const config = await resp.json() as {
        model?: string | null;
        provider?: string | null;
        workspace_id?: string | null;
        reasoning_effort?: string | null;
        temperature?: number | null;
        title?: string | null;
      };

      // ADR-047 P1: Explicitly handle null/empty values to clear stale
      // config from a previous session. Without this, switching to a
      // session with no model set would retain the previous session's
      // model in the UI until the next MQTT push.
      const sessionPatch: Partial<SessionChatState> = {};

      // model: null -> clear, string -> set
      if (typeof config.model === "string" && config.model) {
        sessionPatch.model = config.model;
      } else {
        sessionPatch.model = null;
      }
      // provider: null -> clear, string -> set
      if (typeof config.provider === "string" && config.provider) {
        sessionPatch.provider = config.provider;
      } else {
        sessionPatch.provider = null;
      }
      // reasoning_effort: null -> clear, string -> set
      if (typeof config.reasoning_effort === "string" && config.reasoning_effort) {
        sessionPatch.reasoningEffort = config.reasoning_effort;
      } else {
        sessionPatch.reasoningEffort = null;
      }
      // temperature: null/NaN -> clear, number -> set
      if (typeof config.temperature === "number" && !Number.isNaN(config.temperature)) {
        sessionPatch.temperature = config.temperature;
      } else {
        sessionPatch.temperature = null;
      }

      set((state) => updateSessionState(state, agentId, sessionId, sessionPatch));

      // ADR-047 P2: Apply title from config snapshot. Title lives in
      // agentStore (not SessionChatState) because it's displayed in the
      // sidebar/session list, not the chat panel.
      if (typeof config.title === "string" && config.title) {
        useAgentStore.getState().updateSessionTitle(sessionId, config.title);
      }

      // Workspace selection is owned by workspaceStore, not SessionChatState.
      if (typeof config.workspace_id === "string" && config.workspace_id) {
        useWorkspaceStore.getState().setSessionWorkspaceLocal(sessionId, config.workspace_id);
      }
    } catch (e) {
      log.warn("[ChatStore] fetchSessionConfig failed:", e);
    }
  },

  // ADR-047 section 3.5.2: Combined cold-load function.
  //
  // MUST be used in all switch/open/first-start scenarios.
  // Encapsulates Promise.all of fetchSessionState + fetchSessionConfig
  // so callers cannot accidentally skip one (which would cause config
  // vacuum and bounce-back bug recurrence).
  loadSession: async (agentId: string, sessionId: string) => {
    await Promise.all([
      get().fetchSessionState(agentId, sessionId),
      get().fetchSessionConfig(agentId, sessionId),
    ]);
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

  // ADR-046 §2.5: 5 attachment-type system entries. Each carries a
  // `metadata.type` discriminator matching the backend `AttachmentMeta`
  // serde tag. The raw metadata is stored on `ChatMessage.metadata` so
  // the renderer (MessageBubble) can read it without re-parsing.
  const ATTACHMENT_TYPES = new Set([
    "file_upload",
    "image_upload",
    "attached_file",
    "attached_selection",
    "attached_folder",
  ]);
  if (entry.role === "system" && typeof meta.type === "string" && ATTACHMENT_TYPES.has(meta.type)) {
    base.metadata = meta as Record<string, unknown>;
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
 * Map raw ConversationEntry list to ChatMessage[] — replaces the legacy
 * `mergeDocumentUploads` shim. Post-ADR-046 the runtime no longer writes
 * a separate `document_upload` system entry or appends enriched document
 * text into user content; everything is already inline as
 * `metadata.attached_items` on the user entry itself, and `convertConversationEntry`
 * rehydrates that into `ChatMessage.metadata`. So this function is
 * now a thin map. Kept as a named helper so the callsite stays self-explanatory.
 */
function mergeDocumentUploads(entries: ConversationEntry[], agentId: string): ChatMessage[] {
  return entries.map((e) => convertConversationEntry(e, agentId));
}

// ── WebSocket event handler — routes by event.session_id ──────────────

const CONTENT_EVENT_TYPES = new Set([
  "done", "error", "tool_approval_needed", "ask_question", "iteration_limit_paused",
  "loop_detected_paused",
  "context_usage", "session_state", "stopped", "todo_list_updated",
  "compacting_started", "compacting_ended", "model_confirmed", "reasoning_effort_confirmed",
  "reasoning_started", "reasoning_ended",
  "memory_updated", "skill_executed",
  "session_config", "session_state",
  "stream_delta", "record_complete",
  "tool_progress",
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
  //   log.debug("[DIAG:handleMessageEvent]", eventType, JSON.stringify(data));
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

    // ADR-035 convergent model: stream_delta drives ONLY the
    // isAssistantReplying indicator via activeStreams tracking.  No
    // placeholder is inserted into messages[].  The final content
    // arrives via HTTP refresh triggered by record_complete.
    case "stream_delta": {
      if (!sid) break;
      const lines = (data.lines as Array<{role:string;message_id:string;line_no:number;content:string}>) ?? [];
      log.debug("[ChatStore:DEBUG] stream_delta RECEIVED", {
        sid,
        eventSessionId: data.session_id,
        msgId: lines[0]?.message_id,
        role: lines[0]?.role,
        lineCount: lines.length,
        seq: data.seq,
        activeStreamMessageId: activeStreams.get(sid)?.messageId,
      });
      if (!lines.length) break;
      const role = lines[0].role === 'assistant' ? 'assistant' as const : 'thought' as const;

      if (role === 'thought') {
        // Lightweight tracking for ThinkBlock preview.
        // No real-time content rendering. We track only:
        //   1. isThinking flag (session-level, for ChatPanel)
        //   2. Last 5 lines (for ThinkBlock expand preview)
        //   3. startTime (for timer)
        // thinkingContent is flushed to Zustand state on a 500ms throttle.
        const thoughtMsgId = lines[0].message_id;
      let thoughtStream = activeStreams.get(sid);
      if (!thoughtStream || thoughtStream.messageId !== thoughtMsgId) {
        thoughtStream = { messageId: thoughtMsgId, role: 'thought', lineCount: 0, lines: [], startTime: Date.now() };
        activeStreams.set(sid, thoughtStream);
      }
      for (const l of lines) {
        thoughtStream.lines.push({ role: 'thought', lineNo: l.line_no, content: l.content });
      }
      if (thoughtStream.lines.length > 5) {
        thoughtStream.lines = thoughtStream.lines.slice(-5);
      }
      // Edge-triggered isThinking + startTime
      const thoughtState = getSessionState(get(), agentId, sid);
      if (!thoughtState.isThinking) {
        set((state) => updateSessionState(state, agentId, sid, {
          isThinking: true,
          thinkingStartTime: thoughtStream.startTime,
          thinkingContent: thoughtStream.lines.map(l => l.content).join('\n'),
        }));
        lastThinkingFlush.set(sid, Date.now());
      } else if (thoughtState.isPinnedToBottom) {
        // Throttle: flush at most every 500ms.
        // Only when user is at the bottom (isPinnedToBottom) – when
        // the ThinkBlock is outside the viewport, skip the Zustand
        // set() to avoid unnecessary re-renders.
        const now = Date.now();
        const lastFlush = lastThinkingFlush.get(sid) ?? 0;
        if (now - lastFlush >= 500) {
          const content = thoughtStream.lines.map(l => l.content).join('\n');
          if (content !== thoughtState.thinkingContent) {
            set((state) => updateSessionState(state, agentId, sid, {
              thinkingContent: content,
            }));
          }
          lastThinkingFlush.set(sid, now);
        }
      }
      break;
      }

      // -- assistant --------------------------------------------------
      const msgId = lines[0].message_id;
      let as = activeStreams.get(sid);
      const sessionState = getSessionState(get(), agentId, sid);
      const isFirstChunkForNewStream = !as || as.messageId !== msgId;
      if (isFirstChunkForNewStream) {
        // Edge-triggered: new messageId → reset startTime for duration.
        as = { messageId: msgId, role, lineCount: 0, lines: [], startTime: Date.now() };
        activeStreams.set(sid, as);
        // Seed the throttle timestamp so the first flush below can run
        // immediately (delta from a 0 baseline would otherwise be huge).
        lastAssistantFlush.set(sid, Date.now());
      }
      if (!as) break;
      as.lineCount += lines.length;
      // ADR-035 C2 safety valve: prevent unbounded memory growth if
      // record_complete is lost AND idle realignment fails.
      if (as.lineCount > ASSISTANT_LINE_SAFETY_CAP) {
        log.warn(
          `[ChatStore] ADR-035 C2 safety valve: assistant activeStream hit` +
          ` ${ASSISTANT_LINE_SAFETY_CAP}-line cap (messageId=${as.messageId}).` +
          ` record_complete likely lost - capping lineCount.`,
        );
        as.lineCount = ASSISTANT_LINE_SAFETY_CAP;
      }
      // Mirror the thought branch: keep last 5 lines for the trailing
      // streaming preview (DOM memory stays flat — same <pre> node is
      // reused via direct textContent mutation in StreamingSourceBlock).
      for (const l of lines) {
        as.lines.push({ role: 'assistant', lineNo: l.line_no, content: l.content });
      }
      if (as.lines.length > 5) {
        as.lines = as.lines.slice(-5);
      }
      // Edge-triggered isAssistantReplying flip.
      const shouldBeReplying = as.lineCount > ASSISTANT_REPLYING_LINE_THRESHOLD;
      const isCurrentlyReplying = sessionState.isAssistantReplying;
      if (shouldBeReplying !== isCurrentlyReplying) {
        set((state) => updateSessionState(state, agentId, sid, {
          isAssistantReplying: shouldBeReplying,
        }));
      }
      // Edge-triggered startTime: only push on the first chunk of a new
      // stream so the duration timer starts fresh per assistant message.
      if (isFirstChunkForNewStream) {
        set((state) => updateSessionState(state, agentId, sid, {
          assistantStreamingStartTime: as.startTime,
        }));
      }
      // Throttled flush of the trailing 5-line preview.  Mirror of the
      // thought branch: only push to Zustand when (a) isPinnedToBottom
      // (user is watching the tail) and (b) at least 500ms since the
      // previous flush.  Keeps re-render churn flat regardless of how
      // fast stream_delta events arrive.
      if (sessionState.isPinnedToBottom) {
        const now = Date.now();
        const lastFlush = lastAssistantFlush.get(sid) ?? 0;
        if (now - lastFlush >= 500) {
          const content = as.lines.map((l) => l.content).join('\n');
          if (content !== sessionState.assistantStreamingContent) {
            set((state) => updateSessionState(state, agentId, sid, {
              assistantStreamingContent: content,
            }));
          }
          lastAssistantFlush.set(sid, now);
        }
      }
      break;
    }

    case "record_complete": {
      if (!sid) break;
      log.debug("[ChatStore:DEBUG] record_complete RECEIVED", {
        sid,
        eventSessionId: data.session_id,
        msgId: data.message_id,
        role: data.role,
        seq: data.seq,
        contentLen: typeof data.content === 'string' ? data.content.length : 0,
        activeStreamMessageId: activeStreams.get(sid)?.messageId,
      });
      const rawRole = data.role as string;
      const role = (rawRole === 'assistant' || rawRole === 'thought' || rawRole === 'tool_call' || rawRole === 'tool_result')
        ? rawRole as 'assistant' | 'thought' | 'tool_call' | 'tool_result'
        : 'assistant';
      const msgId = data.message_id as string;
      const toolCallId = (data.tool_call_id as string | undefined) ?? '';

      // Clear isAssistantReplying unconditionally.
      const curSessionState = getSessionState(get(), agentId, sid);
      if (curSessionState.isAssistantReplying) {
        set((state) => updateSessionState(state, agentId, sid, {
          isAssistantReplying: false,
        }));
      }
      // Clear assistantStreamingContent + assistantStreamingStartTime
      // unconditionally — the trailing StreamingSourceBlock variant=
      // "assistant" virtual slot disappears alongside isAssistantReplying,
      // and we don't want stale preview text / startTime surviving a
      // session-id reuse (next stream_delta for a new message would race
      // against a leftover snapshot).
      if (curSessionState.assistantStreamingContent !== ''
        || curSessionState.assistantStreamingStartTime !== null) {
        set((state) => updateSessionState(state, agentId, sid, {
          assistantStreamingContent: '',
          assistantStreamingStartTime: null,
        }));
        lastAssistantFlush.delete(sid);
      }

      // Clear isThinking when thought completes.
      if (role === 'thought' && getSessionState(get(), agentId, sid).isThinking) {
        set((state) => updateSessionState(state, agentId, sid, {
          isThinking: false,
          thinkingContent: '',
        }));
        lastThinkingFlush.delete(sid);
      }

      // Clean up activeStream if it matches.
      const as = activeStreams.get(sid);
      if (as && as.messageId === msgId) {
        activeStreams.delete(sid);
      }

      // Schedule debounced HTTP refresh to load the completed record.
      scheduleRefresh(agentId, sid);

      // ADR-045: Clean up toolProgress entry when a tool_result arrives.
      if (role === 'tool_result') {
        set((state) => {
          const agent = getAgentState(state, agentId);
          const ss = agent.sessionStates[sid!];
          if (!ss) return {};
          const keys = Object.keys(ss.toolProgress);
          if (keys.length === 0) return {};
          const updated = { ...ss.toolProgress };
          if (toolCallId && updated[toolCallId]) {
            delete updated[toolCallId];
          } else {
            // No exact match - clear any entry whose elapsedMs is already
            // >= 90% of its timeout (clearly stale, safe to drop).
            for (const k of keys) {
              const e = updated[k];
              if (e && e.elapsedMs / Math.max(e.timeoutMs, 1) >= 0.9) {
                delete updated[k];
              }
            }
            // If we still have nothing dropped and toolCallId was missing,
            // clear the single oldest entry (deterministic FIFO).
            if (toolCallId === '' && Object.keys(updated).length === keys.length) {
              let oldestId: string | null = null;
              let oldestElapsed = -1;
              for (const k of keys) {
                const e = updated[k];
                if (e && e.elapsedMs > oldestElapsed) {
                  oldestElapsed = e.elapsedMs;
                  oldestId = k;
                }
              }
              if (oldestId) delete updated[oldestId];
            }
          }
          return updateSessionState(state, agentId, sid!, {
            toolProgress: updated,
          });
        });
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
      // Instead, session_state → idle does a single definitive
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
      log.debug("[ChatStore] Model switch confirmed:", confirmedModel, confirmedProvider);
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
      log.debug("[ChatStore] Reasoning effort confirmed:", confirmedEffort);
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
      log.error("[ChatStore] Server error:", errorMsg, errorDetail);
      // ADR-035 Phase 3: no polling
      // Server errors are NOT persisted to JSONL by the backend, so they
      // must NOT enter the messages array (sliding window over JSONL).
      // Store as a separate banner state instead.
      set((state) => ({
        ...updateSessionState(state, agentId, sid!, {
          serverError: {
            content: errorMsg as string,
            errorDetail: errorDetail || undefined,
            errorType: errorType || undefined,
            timestamp: Date.now(),
          },
          isCompacting: false,
        }),
      }));
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
      log.debug("[DIAG:tool_approval_needed]", {
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
          log.debug("[DIAG:tool_approval_needed:set]", {
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
        log.warn("[DIAG:tool_approval_needed] DROPPED — sid is null!");
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
      log.debug("[WS] Memory updated event:", data);
      break;

    case "skill_executed":
      log.debug("[WS] Skill executed event:", data);
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

    case "tool_progress": {
      if (sid) {
        const toolCallId = data.tool_call_id as string;
        const elapsedMs = data.elapsed_ms as number;
        const timeoutMs = data.timeout_ms as number;
        if (toolCallId && elapsedMs != null && timeoutMs != null) {
          set((state) => {
            const agent = getAgentState(state, agentId);
            const ss = agent.sessionStates[sid];
            if (!ss) return {};
            const updated = { ...ss.toolProgress };
            updated[toolCallId] = { elapsedMs, timeoutMs };
            return updateSessionState(state, agentId, sid, {
              toolProgress: updated,
            });
          });
        }
      }
      break;
    }

    case "context_usage": {
      if (sid) {
        // ADR-033: Runtime now publishes a fully-populated `ContextUsageInfo`
        // under `data.context_usage` (serialised from `ContextUsagePayload.context_usage`).
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
          log.debug("[ChatStore] context_usage RECEIVED for agent:", agentId, usage);
          set((state) => updateSessionState(state, agentId, sid, { contextUsage: usage, isCompacting: false }));
        } else {
          log.warn(
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

    case "loop_detected_paused": {
      if (sid) {
        const { message } = data as { message: string };
        set((state) => updateSessionState(state, agentId, sid, {
          loopDetectedPaused: { message },
        }));
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
        log.warn("[ChatStore] fetchSessions after session_created failed:", e);
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
    // session_state events, but having both paths makes the
    // contract observable in the store alone).
    case "session_opened": {
      const sid = data.session_id as string | undefined;
      if (!sid) break;
      const status = data.status as string | undefined;
      const model = typeof data.model === "string" && data.model ? data.model : null;
      const provider = typeof data.provider === "string" && data.provider ? data.provider : null;
      const lastActiveAt =
        typeof data.last_active_at === "string" && data.last_active_at ? data.last_active_at : null;
      log.debug(
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
      log.warn(
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
        log.warn("[ChatStore] fetchSessions after session_deleted failed:", e);
      });
      break;
    }

    // ── Session config (ADR-043: user-configurable fields only) ──
    case "session_config": {
      log.debug("[ChatStore:DEBUG] session_config RECEIVED", {
        sid,
        agentId,
        model_id: data.model_id,
        provider_id: data.provider_id,
        workspace_id: data.workspace_id,
        reasoning_effort: data.reasoning_effort,
        temperature: data.temperature,
        title: data.title,
      });
      if (!sid) break;
      // Title goes into the agent-level sessions[] list (used by the
      // sidebar) so it shows up the moment the retained MQTT message arrives.
      if (typeof data.title === "string" && data.title) {
        useAgentStore.getState().updateSessionTitle(sid, data.title);
      }
      const patch: Partial<SessionChatState> = {};
      if (typeof data.model_id === "string" && data.model_id) patch.model = data.model_id;
      if (typeof data.provider_id === "string" && data.provider_id) patch.provider = data.provider_id;
      if (typeof data.reasoning_effort === "string" && data.reasoning_effort) {
        patch.reasoningEffort = data.reasoning_effort;
      }
      // NaN is the Runtime "no override" sentinel for temperature.
      if (typeof data.temperature === "number" && !Number.isNaN(data.temperature)) {
        patch.temperature = data.temperature;
      }
      if (Object.keys(patch).length > 0) {
        log.debug("[ChatStore:DEBUG] session_config applying patch", { sid, patch });
        set((state) => updateSessionState(state, agentId, sid!, patch));
      }
      // Workspace selection is owned by workspaceStore, not SessionChatState.
      if (typeof data.workspace_id === "string" && data.workspace_id) {
        log.debug("[ChatStore:DEBUG] session_config setting workspace", { sid, workspace_id: data.workspace_id });
        useWorkspaceStore.getState().setSessionWorkspaceLocal(sid, data.workspace_id);
      }
      break;
    }

    // ── Session state (ADR-043: runtime telemetry only) ──
    // Replaces the deleted "session_state" event. Carries status,
    // ratio, context_usage, message_count, tokens, and updated_at.
    // The retained topic ensures reconnecting clients immediately receive
    // the latest state.
    case "session_state": {
      if (sid) {
        const status = data.status as SessionStatus | undefined;
        if (status) {
          const prev = getSessionState(get(), agentId, sid!);
          log.debug(
            `[ChatStore:DEBUG] session_state for ${agentId}/${sid}: ` +
            `prev=${prev.sessionStatus?.status} -> next=${status.status}, ` +
            `messageCount=${prev.messages.length}`,
          );
          set((state) => {
            const sessionPatch: Partial<SessionChatState> = { sessionStatus: status };

            // Runtime fields from SessionState proto.
            if (typeof data.ratio === "number") sessionPatch.ratio = data.ratio as number;
            if (data.context_usage && typeof data.context_usage === "object") {
              sessionPatch.contextUsage = data.context_usage as ContextUsageInfo;
            }

            // ADR-021: Start/stop polling based on status transitions.
            const prev = getSessionState(state, agentId, sid);

            // When status transitions TO Idle from non-Idle, clear pending flags
            if (prev.sessionStatus?.status !== "idle" && status.status === "idle") {
              sessionPatch.pendingApproval = {};
              sessionPatch.pendingQuestions = [];
              sessionPatch.iterationLimitPaused = null;
              sessionPatch.loopDetectedPaused = null;
              sessionPatch.isAssistantReplying = false;
              sessionPatch.isThinking = false;
              sessionPatch.thinkingContent = '';

              // ADR-035 C2/O2: if activeStream still has unfrozen content at
              // idle, record_complete was lost (QoS edge case). Trigger HTTP
              // full realignment to recover.
              const activeStream = activeStreams.get(sid);
              if (activeStream) {
                log.warn(
                  `[ChatStore] ADR-035 C2: activeStream still present at idle` +
                  ` (messageId=${activeStream.messageId}, role=${activeStream.role},` +
                  ` lineCount=${activeStream.lineCount}) - record_complete likely lost,` +
                  ` triggering HTTP realignment`,
                );
                activeStreams.delete(sid);
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
              sessionPatch.retryWaitInfo = null;
              sessionPatch.loopDetectedPaused = null;
            }

            const sessionResult = updateSessionState(state, agentId, sid, sessionPatch);
            let agentStates = sessionResult.agentStates;
            return { agentStates };
          });
        }
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
          log.debug("[ChatStore] Agent config updated:", aid, config);
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
          log.warn("[ChatStore] Failed to parse agent_config JSON:", e);
        }
      }
      break;
    }

    case "sidecar_status": {
      // Sidecar status is currently a debug feature without agent_id routing.
      // The listener at line 609 filters out events without agent_id, so this
      // branch is reached only if agent_id is added to the Rust forwarding.
      log.debug("[ChatStore] Sidecar status:", {
        kind: data.kind,
        endpoint: data.endpoint,
        ready: data.ready,
      });
      break;
    }

    case "memory_node_update": {
      log.debug("[ChatStore] Memory node update:", {
        node_id: data.node_id,
        agent_id: data.agent_id,
      });
      break;
    }

    default:
      log.debug("[ChatStore] Unknown event type:", eventType, data);
  }
}
