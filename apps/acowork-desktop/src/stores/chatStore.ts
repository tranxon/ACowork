import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { ChatMessage, ContextUsageInfo, TokenUsage, ToolApprovalNeededEvent, PaginatedMessages, ConversationEntry, SessionStatus, AskQuestionEvent, ModelEntry, TodoItem, AttachedItem } from "../lib/types";
import { toWireAttachedItems } from "../lib/types";
import { isAtTail } from "../lib/paginationUtils";
import { useAgentStore } from "./agentStore";
import { useGatewayStore } from "./gatewayStore";
import { useUserProfileStore } from "./userProfileStore";
import { useWorkspaceStore } from "./workspaceStore";
import { releaseAdapterSession, clearOptimisticEntries, clearAllOptimisticEntries, ingestStreamDelta, ingestRecordComplete, getChatAdapterSession } from "../components/chat/chatAdapterStore";
import { getGatewayUrl } from "../lib/config";
import { emitAgentConfigRefresh } from "../lib/refresh";
import { sessionConfigToPatch, type SessionConfigInput } from "../lib/sessionConfigMapper";
import { resolveDefaultReasoningEffort } from "../lib/modelCapabilities";
import { with503Retry } from "../lib/httpRetry";
import i18n from "../i18n";
import { showToast } from "../components/common/ToastProvider";
import { log } from "../lib/logger";
import type { LlmAvailability } from "../lib/llmAvailability";
import { llmAvailabilityFromWire } from "../lib/llmAvailability";

// ---------------------------------------------------------------------------
// ADR-050 C2: the per-session active stream tracker, throttle timestamps,
// and the 500ms cadence timer all moved to `chatAdapterStore` (see
// `apps/acowork-desktop/src/components/chat/chatAdapterStore.ts`).
// chatStore no longer mutates streaming state — it forwards stream_delta
// and record_complete to the adapter via `ingestStreamDelta` /
// `ingestRecordComplete` and lets the adapter own the live surface.
// ---------------------------------------------------------------------------



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

// ── Message window merge ──────────────────────────────────────────────
//
// Single source of truth for "what should `messages[]` look like after a
// new HTTP `/sessions/{sid}/messages` response lands?"
//
// Invariants (P0-1, P0-3 from the messages review):
//
//   1. The HTTP response is the authoritative timestamp-ordered window.
//      Any cache entry whose id appears in the response MUST be replaced
//      by the response copy (backend wins for content; ties impossible in
//      practice but we dedupe defensively).
//
//   2. Cache entries with ids NOT in the response are preserved (they
//      belong to a different window — e.g. older entries loaded via
//      `loadPrevPage`). They keep their relative timestamp order.
//
//   3. The final array is sorted by `timestamp` ascending. `Array.sort`
//      is stable in V8 since ES2019, so rows with identical `timestamp`
//      keep their input order — important when attachment system rows and
//      their owning user entry share an ISO millisecond.
//
//   4. No path may wipe `messages[]` based on a failed/aborted HTTP
//      request. Only the response handler may overwrite the window, and
//      only with merged content (never an empty reset).
//
// ADR-050 C2: the optimistic user-message overlay no longer lives in
// chatStore — it is owned by `chatAdapterStore`.  The HTTP response
// handler is responsible for calling `clearOptimisticEntries(agentId,
// sessionId, confirmedIds)` with the set of server-echoed ids so the
// adapter can drop the now-confirmed entries from its overlay.  The
// merge function itself stays a pure `(cache, server) => messages`
// transformation; the overlay is a separate concern.
//
// `mergeMessageWindow` is exported (pure function) so the regression tests
// in `chatStore.test.ts` can pin the contract without needing a Zustand
// store or fetch mock.
export interface MergedMessageWindow {
  /** Sorted, deduped message array ready to write into `messages[]`. */
  messages: ChatMessage[];
}

export function mergeMessageWindow(
  cache: ChatMessage[],
  server: ChatMessage[],
): MergedMessageWindow {
  // Step 1: server is authoritative for its ids. Build a Map<id, msg>
  // starting from server entries so duplicates in `server` itself are
  // resolved in favor of the first occurrence (server is already ts-
  // ordered, so the first occurrence is the oldest; ties are harmless).
  const byId = new Map<string, ChatMessage>();
  for (const m of server) byId.set(m.id, m);

  // Step 2: cache contributes any entry whose id is NOT in the server
  // window. These are older or otherwise out-of-window rows that the
  // server didn't echo back this round (e.g. a paginated window that
  // doesn't overlap the cache's older entries).
  for (const m of cache) {
    if (!byId.has(m.id)) byId.set(m.id, m);
  }

  // Step 3: stable sort by timestamp ascending. The merge Map preserves
  // insertion order on iteration, and Array.sort is stable, so rows
  // sharing an ISO millisecond keep the layered order
  // (server → cache).
  const messages = Array.from(byId.values()).sort(
    (a, b) => a.timestamp - b.timestamp,
  );

  return { messages };
}

/**
 * Write a single user message into the optimistic overlay.
 *
 * ADR-050 C2: this helper is removed.  The optimistic overlay is no
 * longer owned by chatStore — `chatAdapterStore.ingestOptimisticUserMessage`
 * receives the same entry list.  chatStore keeps no per-session
 * optimistic state and the merge function no longer takes an overlay
 * parameter.
 */

// ── ADR-035 C2: assistant activeStream safety valve ──
//
// ADR-050 C2: the active-stream tracker moved to `chatAdapterStore`,
// which now owns the `ASSISTANT_LINE_SAFETY_CAP` /
// `ASSISTANT_REPLYING_LINE_THRESHOLD` constants and the per-session
// stream bookkeeping.  chatStore no longer mutates streaming state.

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
   * ADR-050: **forward (oldest-end) semantics**:
   *
   * - `messageOffset == 0`               → window anchored at the OLDEST entry.
   * - `messageOffset + messageLimit >= messageTotal` → window touches the
   *   NEWEST entry (the cache "is at the tail").
   * - `messageOffset > 0`                → there are older entries beyond the
   *   window's left edge (scroll-up can load them).
   * - `messageOffset + messageLimit < messageTotal` → there are newer entries
   *   beyond the window's right edge (scroll-down can load them).
   *
   * Initial load sets `messageOffset = 0` and `messageLimit = 0`.
   * See `PaginatedMessages` in `lib/types.ts` for the request/response shape.
   */
  messageOffset: number;
  messageLimit: number;
  messageTotal: number;
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

/**
 * ADR-050 C5 reconciliation: replace optimistic entries once the backend
 * persists a send.
 *
 * `sendMessage` inserts the user message + attachment system entries into
 * `messages[]` with `_isOptimistic: true` before the backend has persisted
 * them. The ONLY mechanism that drops `_isOptimistic` is
 * `mergeMessageWindow` id-dedupe during an HTTP refresh — the server copy
 * (same id, no flag) wins. Nothing scheduled that refresh after a send, so
 * a message with attachments kept its "waiting for server confirmation"
 * spinner until the session was reopened.
 *
 * This schedules a short bounded tail-refresh (`loadSessionMessages`). Each
 * landing replaces whatever optimistic entries the server has echoed back;
 * we stop as soon as none remain. The bounded retries cover the backend's
 * asynchronous JSONL writer flush (`Conversation::append_message_with_id`
 * is non-blocking) and the publish → process → persist pipeline.
 */
const SEND_RECONCILE_DELAYS_MS = [300, 800, 1600];

export function scheduleSendReconciliation(agentId: string, sessionId: string, attempt = 0): void {
  if (attempt >= SEND_RECONCILE_DELAYS_MS.length) return;
  globalThis.setTimeout(() => {
    const store = useChatStore.getState();
    const ss = store.getSessionState(agentId, sessionId);
    // Session evicted or every optimistic entry already confirmed — done.
    if (!ss || !ss.messages.some((m) => m._isOptimistic)) return;
    store.loadSessionMessages(agentId, sessionId).then(
      () => {
        const after = useChatStore.getState().getSessionState(agentId, sessionId);
        if (after?.messages.some((m) => m._isOptimistic)) {
          scheduleSendReconciliation(agentId, sessionId, attempt + 1);
        }
      },
      () => scheduleSendReconciliation(agentId, sessionId, attempt + 1),
    );
  }, SEND_RECONCILE_DELAYS_MS[attempt]);
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
    // Release the evicted session's adapter-side live state
    // (activeStream tracker, throttle timestamps, optimisticEntries).
    releaseAdapterSession(agentId, id);
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
  /**
   * LLM availability for the current session, mirrored from the
   * `SessionConfig.llm_availability` retained MQTT topic. Drives the
   * three-state banner in `ChatPanel`.
   *
   * - `unspecified`  — runtime hasn't published yet, render nothing
   * - `loading`      — bootstrap not READY / vault not populated, render placeholder
   * - `configured`   — vault has at least one usable provider, render nothing
   * - `missing`      — vault empty or every provider unusable, render red banner
   */
  llmAvailability: LlmAvailability;

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
    /** When true, REPLACE the cached window with the fetched page instead of merging. */
    replaceCache?: boolean,
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
  llmAvailability: "unspecified",

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
    // ADR-050 C2: optimisticEntries + activeStream + throttle timestamps
    // live in chatAdapterStore; release them here too.
    clearAllOptimisticEntries(agentId, sessionId);
    set((state) => ({
      ...updateSessionState(state, agentId, sessionId, {
        messages: [],
        tokenUsage: null,
        contextUsage: null,
        messageOffset: 0,
        messageLimit: 0,
        messageTotal: 0,
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
    clearAllOptimisticEntries(agentId, sessionId);
    set((state) => ({
      ...updateSessionState(state, agentId, sessionId, {
        messages: [],
        tokenUsage: null,
        contextUsage: null,
        messageOffset: 0,
        messageLimit: 0,
        messageTotal: 0,
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
    releaseAdapterSession(agentId, sessionId);
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

    // ADR-046: Build the unified `attached_items` payload from BOTH upload
    // payloads (`attachedItems`) AND workspace refs (`attachedContext`).
    // They are collected into one list FIRST so that every attachment gets
    // a `clientId` and an optimistic system entry. Without this step
    // workspace refs would only appear after the backend responds.
    const rawItems: AttachedItem[] = [...(attachedItems ?? [])];
    const ssAttachedContext = sessionId
      ? getSessionState(get(), agentId, sessionId).attachedContext
      : [];
    if (sessionId && ssAttachedContext.length > 0) {
      for (const ctx of ssAttachedContext) {
        if (ctx.type === "file") {
          rawItems.push({
            type: "attached_file",
            absPath: ctx.absPath,
            name: ctx.name,
          });
        } else if (ctx.type === "selection") {
          rawItems.push({
            type: "attached_selection",
            absPath: ctx.absPath,
            name: ctx.name,
            startLine: ctx.startLine ?? 1,
            endLine: ctx.endLine ?? ctx.startLine ?? 1,
          });
        } else if (ctx.type === "directory") {
          rawItems.push({
            type: "attached_folder",
            absPath: ctx.absPath,
            name: ctx.name,
          });
        }
      }
      // Clear workspace refs immediately so the next send starts fresh.
      // This must happen before the async MQTT publish to avoid races if
      // the user sends another message quickly.
      set((s) =>
        updateSessionState(s, agentId, sessionId, { attachedContext: [] }),
      );
    }

    const items: AttachedItem[] = rawItems.map((item) => {
      const clientId = `msg-${crypto.randomUUID()}`;
      return { ...item, clientId };
    });

    const now = Date.now();
    const userMsg: ChatMessage = {
      id: userMsgId,
      type: "user",
      content,
      timestamp: now,
      ...getUserSenderInfo(),
    };

    // Build optimistic system entries for each attachment so they appear
    // immediately in the chat list alongside the user text bubble.
    const optimisticAttachments: ChatMessage[] = items.map((item, idx) => {
      const metaType = item.type; // "file_upload" | "image_upload" | "attached_file" | "attached_selection" | "attached_folder"
      const meta: Record<string, unknown> = { type: metaType };
      // Copy all fields except `type` and `clientId` into metadata
      for (const [k, v] of Object.entries(item)) {
        if (k !== "type" && k !== "clientId" && v !== undefined) {
          meta[k] = v;
        }
      }
      return {
        id: item.clientId!,
        type: "system" as const,
        content: "",
        timestamp: now + 1 + idx, // +1ms per attachment so they sort after the user text
        metadata: meta,
        senderDisplayName: undefined,
        _isOptimistic: true as const,
      };
    });

    if (sessionId) {
      // ADR-050 post-C5 fix: user message is written directly into
      // messages[] (optimistic insert).  The id (crypto.randomUUID)
      // survives the round-trip to the backend, so when the HTTP refresh
      // returns the same message, mergeMessageWindow deduplicates by id
      // (server version wins).  This replaces the old liveBuffer.
      // pendingUserMessage path and ensures the user sees their message
      // immediately even in a fresh session where limit === 0.
      set((state) => {
        const ss = getSessionState(state, agentId, sessionId!);
        // Guard against duplicate (rapid double-send of same id).
        if (ss.messages.some(m => m.id === userMsgId)) return {};
        return updateSessionState(state, agentId, sessionId!, {
          messages: [...ss.messages, userMsg, ...optimisticAttachments],
          messageTotal: ss.messageTotal + 1 + optimisticAttachments.length,
          messageLimit: ss.messageLimit + 1 + optimisticAttachments.length,
        });
      });
      log.debug("[ChatStore:DEBUG] sendMessage optimistic insert to messages[]", {
        sid: sessionId,
        userMsgId,
        attachedItemCount: items.length,
      });
    }

    // ADR-046 §params: only `attached_items` survives. The legacy
    // `content_parts` / `attached_context` / `document_ids` MQTT params are
    // dropped — the runtime reads everything it needs from
    // `params_json.attached_items`.
    const params: Record<string, unknown> = {};
    if (items.length > 0) params.attached_items = toWireAttachedItems(items);
    const paramsJson = Object.keys(params).length > 0 ? JSON.stringify(params) : "";

    // Ids of the optimistic entries inserted above — used to roll them back
    // if the MQTT publish fails (the backend never accepted the message).
    const optimisticIds = new Set([userMsgId, ...items.map((i) => i.clientId ?? "")]);

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
      // ADR-050 C5: reconcile the optimistic user message + attachment
      // entries with the server once the backend has persisted them. See
      // `scheduleSendReconciliation` — without this the attachment chips
      // kept their "waiting for server confirmation" spinner until the
      // session was reopened.
      if (sessionId) scheduleSendReconciliation(agentId, sessionId);
    } catch (error) {
      log.error("[ChatStore] MQTT message send failed:", error);
      // The backend never accepted this send — drop the optimistic entries
      // we just inserted so the user does not see a phantom message with a
      // perpetual spinner.
      if (sessionId) {
        set((state) => {
          const ss = getSessionState(state, agentId, sessionId!);
          const removed = ss.messages.filter(
            (m) => optimisticIds.has(m.id) && m._isOptimistic,
          ).length;
          if (removed === 0) return {};
          return updateSessionState(state, agentId, sessionId!, {
            messages: ss.messages.filter(
              (m) => !(optimisticIds.has(m.id) && m._isOptimistic),
            ),
            messageTotal: Math.max(0, ss.messageTotal - removed),
            messageLimit: Math.max(0, ss.messageLimit - removed),
          });
        });
      }
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

    // Resolve new model's default reasoning effort from availableModels.
    // Lookup goes through resolveDefaultReasoningEffort so this stays in
    // sync with the MQTT model_confirmed handler.
    const defaultEffort = resolveDefaultReasoningEffort(get().availableModels, model, provider);

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

  loadSessionMessages: async (
    agentId: string,
    sessionId: string,
    offset?: number,
    limit: number = 50,
    replaceCache = false,
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
    // ADR-050: forward offset semantics.  `offset === undefined` means
    // "from the tail end of the conversation" — backend interprets this
    // via the `tail=true` query parameter so the caller does not need to
    // know `total` before issuing the very first request.
    //
    // offset===0 with a non-empty cache is NOT initial — it's a user-initiated
    // "jump to the oldest page" (a deliberate scroll-to-top), since under
    // forward semantics offset=0 means the OLDEST entry.  We must NOT clear
    // streaming in that case (the agent may still be writing live), and we
    // must NOT show the loading overlay (the cache already has content to
    // render).
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
      // ADR-050: initial load uses `?tail=true` so the backend can return
      // the latest `limit` entries without the caller having to know
      // `total` yet.  Subsequent pagination calls pass an explicit offset.
      if (offset === undefined) {
        params.set("tail", "true");
      } else {
        params.set("offset", String(offset));
      }
      // ADR-035 Phase 3: no HTTP incremental endpoint

      // Bug B v3 fix: chat endpoints proxy through the Runtime and 503
      // during the boot window between Gateway discovery and Runtime
      // HTTP port registration. `with503Retry` honours the Gateway's
      // Retry-After header so a transient 503 at session-load time
      // recovers transparently instead of flashing an error to the user.
      const resp = await with503Retry(
        () => fetch(
          `${getGatewayUrl()}/api/agents/${agentId}/sessions/${sessionId}/messages?${params}`,
          { signal: controller.signal },
        ),
        {
          tag: `ChatStore.loadSessionMessages(${agentId}/${sessionId})`,
          logger: log,
          signal: controller.signal,
        },
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

        // Server window is the authoritative timestamp-ordered slice.
        // `mergeMessageWindow` reconciles it with the local cache and the
        // optimistic-user overlay in a single pass:
        //
        //   1. server entries win on id collision (defensive dedupe even
        //      though the backend never emits duplicates within a window).
        //   2. cache entries with non-overlapping ids are preserved
        //      (older rows from `loadBefore`, etc.).
        //   3. optimistic entries not yet echoed back are kept in the
        //      overlay and sorted by timestamp into the final array.
        //   4. final array is sorted by `timestamp` ascending — this is
        //      what the user perceives as "correct order" regardless of
        //      which window HTTP fetched.
        //
        // Pagination cursor math is unchanged: offset/limit continue to
        // be derived from the server window (server-authoritative).
        // ADR-050: under forward semantics, the direction comparison is
        //   returnedOffset > prevOffset → loading NEWER entries (cursor
        //     walked forward, larger offset = further from oldest)
        //   returnedOffset < prevOffset → loading OLDER entries (cursor
        //     walked back toward the oldest end, smaller offset)
        // The min/max expressions below are direction-agnostic — they
        // always extend the cached range to the union of old + new window.
        // ADR-050 C2: mergeMessageWindow is now (cache, server) → messages
        // (the optimistic overlay lives in chatAdapterStore, not here).
        // We also notify the adapter of which optimistic ids the server
        // confirmed so it can drop them from its overlay.
        const merged = replaceCache
          ? { messages: converted }
          : mergeMessageWindow(ss.messages, converted);
        const confirmedIds = new Set(
          converted.map((m) => m.id),
        );
        // Only run the side effect after the setState closure commits —
        // we don't want a setState on a different store to interleave
        // with this transition.
        queueMicrotask(() => {
          clearOptimisticEntries(agentId, sessionId, confirmedIds);
        });

        let finalOffset = returnedOffset;
        let finalLimit = returnedLimit;

        if (replaceCache) {
          // Jump operation: the fetched page REPLACES the cache.
          // Cursor is exactly what the server returned.
          finalOffset = returnedOffset;
          finalLimit = returnedLimit;
        } else if (isInitialLoad) {
          // ADR-050 C2: drop any in-flight activeStream tracker so a
          // stale lineCount from a previous session incarnation cannot
          // bleed into the freshly-loaded view.  The tracker now lives
          // in chatAdapterStore; chatStore delegates the cleanup.
          releaseAdapterSession(agentId, sessionId);
        } else if (returnedOffset > prevOffset) {
          // Loading NEWER messages (scroll-down).  Window slid forward;
          // extend the cached range to include the newer entries the
          // server just returned.
          finalOffset = Math.min(returnedOffset, prevOffset);
          finalLimit = Math.max(
            returnedOffset + returnedLimit,
            prevOffset + (ss.messages.length > 0 ? prevLimit : 0),
          ) - finalOffset;
        } else if (returnedOffset < prevOffset) {
          // Loading OLDER messages (scroll-up).  Window slid backward
          // (smaller offset under forward semantics); expand the cached
          // range to include the older entries the server just sent.
          finalOffset = Math.min(returnedOffset, prevOffset);
          finalLimit = Math.max(
            returnedOffset + returnedLimit,
            prevOffset + (ss.messages.length > 0 ? prevLimit : 0),
          ) - finalOffset;
        } else {
          // returnedOffset === prevOffset: same window (a duplicate load
          // or a re-fetch of the same page).  No cursor math change.
          finalOffset = Math.min(returnedOffset, prevOffset);
          finalLimit = Math.max(
            returnedOffset + returnedLimit,
            prevOffset + (ss.messages.length > 0 ? prevLimit : 0),
          ) - finalOffset;
        }

        return updateSessionState(state, agentId, sessionId, {
          messages: merged.messages,
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
        // P0-3: abort / cancellation must NEVER wipe the cached message
        // window. The previous implementation reset `messages: []` here,
        // which destroyed in-flight optimistic entries and caused the
        // "user message reappears as duplicate" bug. We only flip the
        // loading flags so the UI can re-attempt via the next scroll /
        // scheduleRefresh / setPinnedToBottom hook.
        log.debug(`[ChatStore] loadSessionMessages aborted (seq ${seq})`);
        set((state) => updateSessionState(state, agentId, sessionId, {
          isLoadingSession: false,
          isLoadingMore: false,
        }));
        return;
      }
      log.error("[ChatStore] Failed to load session messages:", e);
      // P0-3: even on hard failures, do NOT reset `messages[]`. The
      // cache may already hold valid content from a previous window; we
      // only surface the error and clear loading flags so the retry
      // button in VirtualMessageList can fire `ensureLatestInCache`
      // without first losing the cached history.
      set((state) => updateSessionState(state, agentId, sessionId, {
        isLoadingSession: false,
        isLoadingMore: false,
        loadError: `${i18n.t("chatPanel.sessionLoadFailed")}: ${e instanceof Error ? e.message : String(e)}`,
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
   * ADR-050: One-shot jump to the latest page (tail of the conversation).
   *
   * Replaces the old `loadMoreNewerMessages` loop which had to issue N HTTP
   * requests to slide a cache from the middle to the tail.  After this call
   * the cache holds the newest page; `messageOffset + messageLimit ===
   * messageTotal`; the rendering layer's ensureRenderable effect then decides
   * if more data is needed to fill the viewport (same load path).
   *
   * No-op if the cache is already at the tail (the cache window's far edge
   * touches `total`) — the caller is expected to check before invoking.
   */
  ensureLatestInCache: async (agentId: string, sessionId: string) => {
    const sessionState = getSessionState(get(), agentId, sessionId);
    if (sessionState.isLoadingMore) return;
    const { messageOffset, messageLimit, messageTotal, messages } = sessionState;
    // Already at the tail:
    //   - The cache window's far edge touches the end of the conversation
    //     (messageOffset + messageLimit >= messageTotal), AND
    //   - messageTotal > 0 (we know the conversation has data — guards against
    //     a freshly-initialized sessionState whose DEFAULT values are all 0 /
    //     empty, which would otherwise be mistaken for "already at tail"), AND
    //   - messages.length > 0 (the cache has at least some data at the tail).
    const tailCovered =
      messageTotal > 0 &&
      messageOffset + messageLimit >= messageTotal &&
      messages.length > 0;
    if (tailCovered) return;
    set((state) => updateSessionState(state, agentId, sessionId, { isLoadingMore: true }));
    try {
      // Forward semantics: the latest `limit` entries live at
      // `offset = max(0, total - limit)`.  When total === 0 (fresh session
      // that has never been loaded) we delegate to the initial-load path,
      // which uses `?tail=true` to grab the newest `limit` entries.
      const limit = 50;
      if (messageTotal === 0) {
        await get().loadSessionMessages(agentId, sessionId, undefined, limit);
      } else {
        const tailOffset = Math.max(0, messageTotal - limit);
        await get().loadSessionMessages(agentId, sessionId, tailOffset, limit);
      }
    } finally {
      set((state) => updateSessionState(state, agentId, sessionId, { isLoadingMore: false }));
    }
  },

  /** Release the messages array for a session to free memory. */
  clearSessionMessages: (agentId: string, sessionId: string) => {
    clearAllOptimisticEntries(agentId, sessionId);
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
      // Bug B v3 fix: see `loadSessionMessages` for the rationale.
      // We pass the AbortSignal through `with503Retry` so the chat
      // panel can cancel the boot-window retry loop when the user
      // switches sessions or closes the panel.
      const resp = await with503Retry(
        () => fetch(
          `${getGatewayUrl()}/api/agents/${agentId}/sessions/${sessionId}`,
        ),
        { tag: `ChatStore.fetchSessionState(${agentId}/${sessionId})`, logger: log },
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
      // Bug B v3 fix: same 503 retry rationale as fetchSessionState.
      const resp = await with503Retry(
        () => fetch(
          `${getGatewayUrl()}/api/agents/${agentId}/sessions/${sessionId}/config`,
        ),
        { tag: `ChatStore.fetchSessionConfig(${agentId}/${sessionId})`, logger: log },
      );
      if (!resp.ok) {
        log.warn(`[ChatStore] fetchSessionConfig HTTP ${resp.status} for session ${sessionId}`);
        return;
      }
      const config = await resp.json() as SessionConfigInput & {
        workspace_id?: string | null;
        title?: string | null;
      };

      // ADR-047 P1: null/empty values clear stale config from the previous
      // session. The mapper is the single source of truth for both HTTP
      // and MQTT paths - see sessionConfigMapper.ts. Both paths use
      // clearOnNull: true because both deliver a full snapshot.
      const sessionPatch = sessionConfigToPatch(config, { clearOnNull: true });

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

/**
 * Convert an MQTT `record_complete` payload into a `ChatMessage` ready for
 * direct insertion into `messages[]`.
 *
 * ADR-050 post-C5 fix: record_complete carries the COMPLETE content for
 * assistant / thought / tool_call / tool_result records.  Instead of
 * waiting for an HTTP refresh to surface these, we write them directly
 * into `messages[]` so the user sees the finished message immediately.
 *
 * The MQTT payload mirrors the JSONL entry fields (role / message_id /
 * content / tool_name / tool_call_id / is_error / seq) but is NOT a
 * `ConversationEntry` - it lacks `ts` and `metadata`.  We synthesize a
 * `timestamp` from `Date.now()` (clamped to be strictly greater than the
 * last cached message's timestamp so `foldMessages` sort order is
 * preserved) and reconstruct the `toolData` / `toolStatus` fields from
 * the flat MQTT fields.
 */
function convertRecordCompleteToChatMessage(
  data: Record<string, unknown>,
  agentId: string,
  lastTimestamp: number,
  // ADR-050 post-C5 fix: when set, these are written onto thought
  // messages so the renderer (PairedExploreItem / StreamingSourceBlock)
  // can distinguish a completed thought (`endTime` set) from a still-
  // streaming one.  Without `endTime`, a thought written to messages[]
  // by `record_complete` was indistinguishable from an in-flight stream
  // (PairedExploreItem's `isStreaming && !item.msg.endTime` evaluated
  // true forever, so the block stayed expanded and never auto-folded).
  // The `startTime` is sourced from chatAdapterStore's liveBuffer
  // `thinkingStream` entry (set when the first `stream_delta` for that
  // messageId landed).  The `endTime` falls back to `Date.now()` if the
  // MQTT payload doesn't carry an explicit end timestamp.
  thoughtTiming?: { startTime?: number | null; endTime?: number | null },
): ChatMessage {
  const role = data.role as string;
  const msgId = data.message_id as string;
  const content = (data.content as string) ?? "";
  const toolName = data.tool_name as string | undefined;
  const toolCallId = data.tool_call_id as string | undefined;
  const isError = data.is_error as boolean | undefined;
  const seq = data.seq as number | undefined;

  const agentInfo = getAgentSenderInfo(agentId);

  const msg: ChatMessage = {
    id: msgId,
    type: (role === "think" ? "thought" : role) as ChatMessage["type"],
    content,
    // Clamp to lastTimestamp + 1 so the new entry sorts AFTER every
    // cached message in foldMessages' timestamp-ascending sort.
    timestamp: Math.max(Date.now(), lastTimestamp + 1),
    senderDisplayName: agentInfo.senderDisplayName,
    senderRole: agentInfo.senderRole,
  };
  if (seq != null) msg.seq = seq;

  if (role === "tool_call" || role === "tool_result") {
    msg.toolName = toolName;
    msg.toolCallId = toolCallId;
    msg.toolData = {
      tool_name: toolName,
      tool_call_id: toolCallId,
      is_error: isError,
    } as Record<string, unknown>;
    if (role === "tool_result") {
      msg.toolStatus = isError ? "error" : "success";
    }
  }

  // ADR-050 post-C5 fix: stamp startTime/endTime on completed thoughts so
  // the rendering layer can auto-fold them.  See comment on the
  // `thoughtTiming` parameter above for the root-cause analysis.
  if (role === "think" || role === "thought") {
    if (thoughtTiming?.startTime != null) msg.startTime = thoughtTiming.startTime;
    if (thoughtTiming?.endTime != null) msg.endTime = thoughtTiming.endTime;
    else msg.endTime = Date.now();
  }

  return msg;
}

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
  "done", "error", "tool_approval_needed", "ask_question",
  "context_usage", "session_state", "stopped", "todo_list_updated",
  "compacting_started", "compacting_ended", "model_confirmed", "reasoning_effort_confirmed",
  "reasoning_started", "reasoning_ended",
  "memory_updated", "skill_executed",
  "session_config", "session_state",
  "stream_delta", "record_complete",
  "tool_progress",
]);


/**
 * Process a single MQTT agent-event payload. Exported for unit testing;
 * production code should route events through the MQTT listener.
 */
export function handleMessageEvent(
  data: Record<string, unknown>,
  set: (fn: Partial<ChatStore> | ((state: ChatStore) => Partial<ChatStore>)) => void,
  get: () => ChatStore,
  agentId: string,
) {
  const eventType = data.type as string;
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

    // ADR-050 C2: stream_delta absorption lives in `chatAdapterStore`.
    // chatStore no longer mutates per-session streaming state — the
    // adapter owns the activeStream tracker, throttle timestamps,
    // isThinking/isAssistantReplying flags, and the trailing preview
    // content.  This handler is a one-line forwarder that hands the
    // raw `lines` array to the adapter's ingest entry point.
    case "stream_delta": {
      if (!sid) break;
      const lines = (data.lines as Array<{role:string;message_id:string;line_no:number;content:string}>) ?? [];
      log.debug("[ChatStore:DEBUG] stream_delta RECEIVED (forwarded to adapter)", {
        sid,
        eventSessionId: data.session_id,
        msgId: lines[0]?.message_id,
        role: lines[0]?.role,
        lineCount: lines.length,
        seq: data.seq,
      });
      if (!lines.length) break;
      ingestStreamDelta(agentId, sid, lines);
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
      });
      const rawRole = data.role as string;
      const role = (rawRole === 'assistant' || rawRole === 'thought' || rawRole === 'tool_call' || rawRole === 'tool_result')
        ? rawRole as 'assistant' | 'thought' | 'tool_call' | 'tool_result'
        : 'assistant';
      const msgId = data.message_id as string;
      const toolCallId = (data.tool_call_id as string | undefined) ?? '';

      // ADR-050 post-C5 fix: record_complete carries the COMPLETE content.
      // Write it directly into messages[] when atTail so the user sees the
      // finished message immediately, without waiting for an HTTP refresh.
      // This closes the "tool_call/tool_result not visible during streaming"
      // gap and eliminates the need for scheduleRefresh.
      //
      // atTail uses the shared `isAtTail()` function (same definition as
      // chatListAdapter's buildSnapshot) so that `limit === 0` (fresh
      // session before initial HTTP load) is also treated as at-tail.
      // This ensures record_complete writes to messages[] even before the
      // first HTTP response arrives - the message will be deduped by
      // mergeMessageWindow when the HTTP response eventually lands.
      {
        const ss = getSessionState(get(), agentId, sid);
        const atTail = isAtTail(ss.messageOffset, ss.messageLimit, ss.messageTotal);
        if (atTail) {
          const lastTs = ss.messages.length > 0
            ? ss.messages[ss.messages.length - 1].timestamp
            : 0;
          // ADR-050 post-C5 fix: when this record_complete closes a thought,
          // stamp startTime/endTime on the converted message so the renderer
          // can auto-fold the thought block.  startTime comes from the
          // adapter's liveBuffer thinkingStream entry (set when the matching
          // stream_delta first landed).  We read it from there rather than
          // from `thinkingStartTime`, because the latter is a legacy
          // projection that is only reset on the next thought's first delta
          // and can leak a stale value from a previous thought cycle.
          // endTime defaults to "now" — close enough for the duration display,
          // which is rounded to seconds.
          let thoughtTiming: { startTime?: number | null; endTime?: number | null } | undefined;
          if (role === "thought") {
            const adapter = getChatAdapterSession(agentId, sid);
            const liveThought = adapter.liveBuffer.thinkingStream;
            thoughtTiming = {
              startTime: liveThought?.id === msgId ? liveThought.startTime : null,
              endTime: Date.now(),
            };
          }
          const chatMsg = convertRecordCompleteToChatMessage(data, agentId, lastTs, thoughtTiming);
          set((state) => {
            const ss2 = getSessionState(state, agentId, sid!);
            // Dedup: if the id is already in messages[] (e.g. MQTT QoS 1
            // duplicate delivery, or HTTP refresh already landed), skip.
            if (ss2.messages.some(m => m.id === msgId)) return {};
            return updateSessionState(state, agentId, sid!, {
              messages: [...ss2.messages, chatMsg],
              messageTotal: ss2.messageTotal + 1,
              messageLimit: ss2.messageLimit + 1,
            });
          });
        }
      }

      // Clear the corresponding liveBuffer stream (thinkingStream /
      // assistantStream).  Only clears - does NOT push into
      // pendingRecordComplete (that concept is removed in the post-C5 fix).
      ingestRecordComplete(agentId, sid, { messageId: msgId, role });

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
        // Same lookup as setCurrentModel -- see resolveDefaultReasoningEffort.
        const defaultEffort = resolveDefaultReasoningEffort(get().availableModels, confirmedModel, confirmedProvider ?? "");

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
      if (sid) {
        const approvalEvent = data as unknown as ToolApprovalNeededEvent;
        set((state) => {
          const agentState = state.agentStates[agentId];
          const prevPending = agentState?.sessionStates[sid]?.pendingApproval || {};
          const key = approvalEvent.tool_call_id || approvalEvent.request_id;
          const newPending = { ...prevPending, [key]: approvalEvent };
          return updateSessionState(state, agentId, sid, {
            pendingApproval: newPending,
          });
        });
      } else {
        log.warn("[ChatStore] tool_approval_needed dropped - session_id is null", {
          agentId,
          tool_call_id: data.tool_call_id,
          request_id: data.request_id,
        });
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
      // ADR-014: Pause UX is derived from session_state (Paused detail with
      // reason/message). This transient event is intentionally ignored —
      // the frontend no longer mirrors pause flags from separate channels.
      break;
    }

    case "loop_detected_paused": {
      // ADR-014: same as iteration_limit_paused — derived from session_state.
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
      const beforeActive = getAgentState(get(), agentId).activeSessionId;
      log.info("[ChatStore] session_created received", {
        newSessionId,
        beforeActive,
        beforeOpenSessionIds: getAgentState(get(), agentId).openSessionIds,
      });
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
      // Diagnostic: log AFTER state so we can verify the activation ran.
      // setTimeout(0) ensures the synchronous set() inside openSession
      // has settled before we read state.
      setTimeout(() => {
        const after = getAgentState(useChatStore.getState(), agentId);
        log.info('[ChatStore] session_created handled', {
          newSessionId,
          afterActive: after.activeSessionId,
          afterOpenSessionIds: after.openSessionIds,
        });
      }, 0);
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
      // MQTT session_config envelope uses model_id / provider_id and encodes
      // "no override" as "" for strings / NaN for floats (prost can't
      // encode Option<T>). Normalize to the unified SessionConfigInput
      // shape and delegate to the mapper. clearOnNull: true because the
      // retained session_config is a full snapshot - every field is
      // always present, and a null/empty value means "session has no
      // override" which must clear any stale value from the UI.
      const mqttConfig: SessionConfigInput = {
        model: typeof data.model_id === "string" && data.model_id ? data.model_id : null,
        provider: typeof data.provider_id === "string" && data.provider_id ? data.provider_id : null,
        reasoning_effort: typeof data.reasoning_effort === "string" && data.reasoning_effort ? data.reasoning_effort : null,
        temperature: typeof data.temperature === "number" && !Number.isNaN(data.temperature) ? data.temperature : null,
      };
      const patch = sessionConfigToPatch(mqttConfig, { clearOnNull: true });
      if (Object.keys(patch).length > 0) {
        log.debug("[ChatStore:DEBUG] session_config applying patch", { sid, patch });
        set((state) => updateSessionState(state, agentId, sid!, patch));
      }
      // LLM availability is a global-runtime signal; store at the top
      // level (not per-session) since every session of the same agent
      // sees the same value. `data.llm_availability` is the protobuf
      // wire field (i32 / enum string from JSON conversion).
      const nextAvail = llmAvailabilityFromWire(data.llm_availability);
      // `unspecified` means "this message carries no availability info"
      // (old runtime without the field, or a per-session config
      // re-publish that wasn't tagged). Never downgrade a known state
      // back to unspecified — that would hide the banner until the next
      // availability transition fires.
      if (nextAvail !== "unspecified" || get().llmAvailability === "unspecified") {
        if (nextAvail !== get().llmAvailability) {
          set({ llmAvailability: nextAvail });
        }
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

            // When status transitions TO Idle from non-Idle, clear pending flags.
            // ADR-050 C2: isAssistantReplying / isThinking / thinkingContent
            // live in chatAdapterStore now.  The adapter also owns the
            // activeStream tracker, so the "record_complete lost" recovery
            // path delegates to `releaseAdapterSession` to drop the stale
            // tracker before the HTTP realignment fires.
            if (prev.sessionStatus?.status !== "idle" && status.status === "idle") {
              sessionPatch.pendingApproval = {};
              sessionPatch.pendingQuestions = [];

              // ADR-035 C2/O2: if activeStream still has unfrozen content
              // at idle, record_complete was lost (QoS edge case). Trigger
              // HTTP full realignment to recover.  The tracker itself now
              // lives in chatAdapterStore; we delegate cleanup to the
              // adapter and then run the realignment from chatStore.
              const adapter = getChatAdapterSession(agentId, sid);
              if (adapter.isThinking || adapter.isAssistantReplying) {
                log.warn(
                  `[ChatStore] ADR-035 C2: adapter still streaming at idle ` +
                  `(isThinking=${adapter.isThinking}, isAssistantReplying=${adapter.isAssistantReplying}) ` +
                  ` - record_complete likely lost, triggering HTTP realignment`,
                );
                releaseAdapterSession(agentId, sid);
                queueMicrotask(() => {
                  get().loadSessionMessages(agentId, sid);
                });
              }
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
        // `sleeping` is included by the plain-text branch (Desktop
        // `parse_plaintext_agent_status`) and by the protobuf branch
        // (newer Runtimes / Gateway republish). Older agents may omit
        // it; `updateAgentOnlineStatus` defaults `sleeping` to false.
        const sleeping = (data as { sleeping?: boolean }).sleeping ?? false;
        useAgentStore.getState().updateAgentOnlineStatus(aid, online, sleeping);
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

          // Sync context_window to all cached session contextUsage.
          //
          // The agent_config MQTT event carries the full AgentConfig
          // (including context_window) as a retained message, published
          // immediately after PUT /api/agents/{id}/config.  However the
          // live contextUsage push (via session_state / messages/context_usage)
          // only fires when the agent loop runs.  Without this local sync,
          // the ContextUsageIcon, status bar, and ResultsPanel keep showing
          // the old context_window until the user sends a message and
          // triggers the loop.
          //
          // We update context_window and recalculate the derived fields
          // (usage_percent, usable_context) as a best-effort approximation.
          // The next agent-loop context_usage push will overwrite these
          // with the backend's exact computation (which accounts for model
          // capabilities and output token reservation).
          if (typeof config.context_window === "number" && config.context_window > 0) {
            const newWindow = config.context_window;
            set((state) => {
              const agent = state.agentStates[aid];
              if (!agent) return {};
              let changed = false;
              const updatedSessions = { ...agent.sessionStates };
              for (const [sid, sess] of Object.entries(updatedSessions)) {
                if (!sess.contextUsage) continue;
                const cu = sess.contextUsage;
                const total = cu.total_tokens ?? 0;
                updatedSessions[sid] = {
                  ...sess,
                  contextUsage: {
                    ...cu,
                    context_window: newWindow,
                    usage_percent: Math.min(100, Math.round((total / newWindow) * 100)),
                    usable_context: Math.max(0, newWindow - total),
                  },
                };
                changed = true;
              }
              return changed ? updateAgentState(state, aid, { sessionStates: updatedSessions }) : {};
            });
          }
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
