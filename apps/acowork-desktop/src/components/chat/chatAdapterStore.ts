/**
 * chatAdapterStore - Module-level zustand store backing the live stream
 * surface that used to live inside chatStore.
 *
 * Background
 * ----------
 * Pre-ADR-050, the chatStore carried every "is the model currently
 * streaming?" flag (isAssistantReplying / isThinking / thinkingContent /
 * assistantStreamingContent / isPinnedToBottom) AND a per-session
 * optimisticEntries overlay.  That worked, but it coupled three distinct
 * concerns into one store:
 *
 *   1. Server-authoritative history (`messages[]`).
 *   2. Real-time stream absorption (delta coalescing, throttle, preview).
 *   3. UI scroll-position state (pinned-to-bottom / jump target).
 *
 * ADR-050 splits these three concerns:
 *
 *   - chatStore        → only (1) — server-authoritative history.
 *   - chatAdapterStore → (2) + (3) — live data, plus any UI scroll
 *                         primitives the adapter exposes to ChatPanel
 *                         (jumpToLatest / jumpToOldest, etc.).
 *
 * C2 scope (this commit)
 * ----------------------
 * C2 lands the *data plumbing* and keeps the *consumer shape* identical
 * to the pre-ADR-050 store.  Concretely:
 *
 *   - The state shape here still exposes `isThinking / thinkingContent /
 *     assistantStreamingContent / isPinnedToBottom` and the pending
 *     optimistic message list, so existing consumers (ChatPanel, VML,
 *     ExploreBlock) can keep reading them via subscriptions.
 *   - The ingest API (`ingestOptimisticUserMessage` / `ingestStreamDelta`
 *     / `ingestRecordComplete`) is the single entry point that chatStore
 *     uses to push live data.  Throttling, preview-capping, and the
 *     pinned-to-bottom signal live here.
 *   - The subscribe API is the foundation C3/C4 will build on for
 *     event-driven refresh and scroll-controller event subscriptions.
 *
 * C3 will replace the per-field state with a real `liveBuffer` (folded
 * into MessageBlock via the v2 adapter) — at that point the per-field
 * getters will start returning projections off the buffer.
 */
import { create } from "zustand";
import type { ChatMessage, StreamLine } from "../../lib/types";

// ── Constants ──────────────────────────────────────────────────────────────

/**
 * Minimum accumulated streaming lines before the per-session
 * `isAssistantReplying` flag flips true.  Mirrors the pre-ADR-050
 * `ASSISTANT_REPLYING_LINE_THRESHOLD` constant in chatStore.  Kept here
 * (not in chatStore) because the threshold belongs to "how do we
 * absorb the stream", not to "how do we represent the cache".
 */
const ASSISTANT_REPLYING_LINE_THRESHOLD = 3;

/**
 * Assistant content is NEVER truncated for display.  This cap is a
 * safety valve only — set far above any realistic assistant reply.
 * When triggered, it indicates a bug in the record_complete delivery
 * path; a warning is logged.
 */
const ASSISTANT_LINE_SAFETY_CAP = 10_000;

/** Preview buffer cap: trailing N lines for both thought and assistant. */
const PREVIEW_LINE_CAP = 5;

/** Stream flush throttle — bound Zustand set() calls during streaming. */
const STREAM_FLUSH_THROTTLE_MS = 500;

// ── Types ──────────────────────────────────────────────────────────────────

/** Per-session state owned by chatAdapterStore.  C3 splits the
 *  surface into two layers:
 *
 *  - **liveBuffer**: structured ChatMessage[] entries (4 fields) that
 *    the v2 adapter folds into MessageBlock (marked `isLive: true`).
 *  - **legacy projection fields**: text / timestamp previews kept
 *    so C2 consumers (ChatPanel → VML → StreamingSourceBlock) keep
 *    working without a behavior change.  C5 will drop these when
 *    the rendering layer fully consumes `adapter.blocks`.
 */
export interface LiveBuffer {
  /**
   * Mid-stream thought (reasoning) text.  Populated as a single
   * rolling ChatMessage keyed by `messageId`.  Cleared on
   * `record_complete (role: thought)` - the completed record is
   * written directly into `messages[]` by chatStore.
   */
  thinkingStream: ChatMessage | null;
  /**
   * Mid-stream assistant text.  Populated as a single rolling
   * ChatMessage keyed by `messageId`.  Cleared on
   * `record_complete (role: assistant)` - the completed record is
   * written directly into `messages[]` by chatStore.
   */
  assistantStream: ChatMessage | null;
}

export interface AdapterSessionState {
  // ── liveBuffer (v2 adapter primary source) ──
  /** The 4-field live buffer — see {@link LiveBuffer}. */
  liveBuffer: LiveBuffer;

  // ── legacy projection fields (C2 compat) ──
  /** True while the agent is in the thinking/reasoning phase. */
  isThinking: boolean;
  /** Timestamp when the current thinking phase started. */
  thinkingStartTime: number | null;
  /** Latest thought lines (cap 5, joined) for ThinkBlock preview. */
  thinkingContent: string;
  /**
   * Latest assistant text lines (joined) for the trailing streaming
   * preview rendered as a `StreamingSourceBlock variant="assistant"`
   * virtual item.  Capped at the source (last 5 lines) to keep DOM
   * memory flat during long streams.
   */
  assistantStreamingContent: string;
  /** Timestamp when the current assistant stream started. */
  assistantStreamingStartTime: number | null;
  /** True while the assistant is actively streaming AND has accumulated
   *  more than `ASSISTANT_REPLYING_LINE_THRESHOLD` lines. */
  isAssistantReplying: boolean;
  /**
   * Unconfirmed user messages (text + attachment system entries)
   * written by `sendMessage` and dropped when the matching server
   * record arrives (same `id`).
   *
   * ADR-050 C3: prefer `liveBuffer.pendingUserMessage` (single
   * rolling entry).  This field is kept as the multi-entry
   * projection for C2 consumers (ChatPanel builds a
   * `[messages, optimisticEntries]` display array).
   */
  optimisticEntries: ChatMessage[];
}

const DEFAULT_LIVE_BUFFER: LiveBuffer = {
  thinkingStream: null,
  assistantStream: null,
};

const DEFAULT_ADAPTER_SESSION_STATE: AdapterSessionState = {
  liveBuffer: { ...DEFAULT_LIVE_BUFFER },
  // Legacy fields (C2 compat)
  isThinking: false,
  thinkingStartTime: null,
  thinkingContent: "",
  assistantStreamingContent: "",
  assistantStreamingStartTime: null,
  isAssistantReplying: false,
  optimisticEntries: [],
};

/** Event payload emitted by `subscribe()`.  ADR-050 §3.5: C3 will use
 *  these to drive the v2 adapter's refresh and scroll primitives. */
export type ChatAdapterEvent =
  | { kind: "liveUpdate"; sessionKey: string }   // a stream_delta landed
  | { kind: "recordComplete"; sessionKey: string } // a record landed
  | { kind: "flushAvailable"; sessionKey: string } // pending optimistic flushed
  | { kind: "pageLoaded"; sessionKey: string };    // HTTP refresh landed

type Listener = (event: ChatAdapterEvent) => void;

interface ChatAdapterState {
  /** Per-session live state.  Key = `${agentId}:${sessionId}`. */
  sessions: Record<string, AdapterSessionState>;
}

// ── Module-level state (not zustand state) ────────────────────────────────
//
// These Maps hold the active-stream tracker + throttle timestamps.  They
// were previously in chatStore.ts (activeStreams / lastThinkingFlush /
// lastAssistantFlush).  They live here because they are pure absorption
// bookkeeping — they have no React-rendering role and don't need to be
// inside the zustand store.

interface ActiveStreamTracker {
  messageId: string;
  role: "thought" | "assistant";
  lineCount: number;
  lines: StreamLine[];
  startTime: number;
}
const activeStreams = new Map<string, ActiveStreamTracker>();

const lastThinkingFlush = new Map<string, number>();
const lastAssistantFlush = new Map<string, number>();

// ── Store ──────────────────────────────────────────────────────────────────

export const useChatAdapterStore = create<ChatAdapterState>(() => ({
  sessions: {},
}));

const listeners = new Set<Listener>();

/**
 * Subscribe to chatAdapterStore events.  Used by the v2 adapter (C3)
 * to react to live updates without each consumer wiring its own zustand
 * subscription.  Returns an unsubscribe function.
 */
export function subscribeChatAdapter(cb: Listener): () => void {
  listeners.add(cb);
  return () => {
    listeners.delete(cb);
  };
}

function emit(event: ChatAdapterEvent): void {
  for (const cb of listeners) {
    try {
      cb(event);
    } catch (err) {
      // A misbehaving listener must not break the absorption pipeline.
      // Log to console rather than throwing — the stream is more
      // important than any single subscriber.
      // eslint-disable-next-line no-console
      console.error("[chatAdapterStore] subscriber threw:", err);
    }
  }
}

// ── Helpers ────────────────────────────────────────────────────────────────

function sessionKey(agentId: string, sessionId: string): string {
  return `${agentId}:${sessionId}`;
}

function getOrCreate(state: ChatAdapterState, key: string): {
  next: ChatAdapterState;
  session: AdapterSessionState;
} {
  const existing = state.sessions[key];
  if (existing) {
    return { next: state, session: existing };
  }
  const created: AdapterSessionState = { ...DEFAULT_ADAPTER_SESSION_STATE };
  return {
    next: { ...state, sessions: { ...state.sessions, [key]: created } },
    session: created,
  };
}

function patchSession(
  key: string,
  patch: Partial<AdapterSessionState>,
): AdapterSessionState {
  let result: AdapterSessionState | null = null;
  useChatAdapterStore.setState((state) => {
    const { next, session } = getOrCreate(state, key);
    const merged: AdapterSessionState = { ...session, ...patch };
    result = merged;
    return { ...next, sessions: { ...next.sessions, [key]: merged } };
  });
  // setState is synchronous; result is guaranteed populated.
  return result!;
}

function readSession(key: string): AdapterSessionState {
  const state = useChatAdapterStore.getState();
  return state.sessions[key] ?? DEFAULT_ADAPTER_SESSION_STATE;
}

// ── Ingest API (the single entry point for live data) ─────────────────────

/**
 * Push a freshly-sent user message into the optimistic overlay.
 *
 * ADR-050 post-C5 fix: the user message is now written directly into
 * `messages[]` by `chatStore.sendMessage`.  This function is kept as a
 * no-op stub for backward compatibility with any caller that still
 * references it, but it no longer modifies `liveBuffer` (which only
 * stores streaming previews).
 */
export function ingestOptimisticUserMessage(
  _agentId: string,
  _sessionId: string,
  _entries: ChatMessage[],
): void {
  // No-op: user messages are written directly to messages[] by chatStore.
}

/**
 * Remove an optimistic entry by id (used when the server confirms the
 * matching user record via HTTP refresh).
 */
export function clearOptimisticEntries(
  agentId: string,
  sessionId: string,
  ids: ReadonlySet<string>,
): void {
  if (ids.size === 0) return;
  const key = sessionKey(agentId, sessionId);
  const current = readSession(key);
  const remaining = current.optimisticEntries.filter((m) => !ids.has(m.id));
  if (remaining.length === current.optimisticEntries.length) return;
  patchSession(key, {
    optimisticEntries: remaining,
  });
}

/** Drop every optimistic entry for a session.  Used on session close. */
export function clearAllOptimisticEntries(agentId: string, sessionId: string): void {
  const key = sessionKey(agentId, sessionId);
  const current = readSession(key);
  if (current.optimisticEntries.length === 0) return;
  patchSession(key, {
    optimisticEntries: [],
  });
}

/**
 * Ingest a batch of `stream_delta` lines.
 * Mirrors the pre-ADR-050 chatStore behaviour: track the per-messageId
 * active stream, throttle preview flushes to 500ms, edge-trigger the
 * thinking/replying flags.  All state writes go to chatAdapterStore —
 * chatStore no longer touches these fields.
 */
export function ingestStreamDelta(
  agentId: string,
  sessionId: string,
  lines: ReadonlyArray<{ role: string; message_id: string; line_no: number; content: string }>,
): void {
  if (lines.length === 0) return;
  const key = sessionKey(agentId, sessionId);
  const role = lines[0].role === "assistant" ? "assistant" : "thought";
  const msgId = lines[0].message_id;
  const now = Date.now();

  if (role === "thought") {
    let stream = activeStreams.get(key);
    if (!stream || stream.messageId !== msgId) {
      stream = { messageId: msgId, role: "thought", lineCount: 0, lines: [], startTime: now };
      activeStreams.set(key, stream);
    }
    for (const l of lines) {
      stream.lines.push({ role: "thought", lineNo: l.line_no, content: l.content });
    }
    if (stream.lines.length > PREVIEW_LINE_CAP) {
      stream.lines = stream.lines.slice(-PREVIEW_LINE_CAP);
    }
    const current = readSession(key);
    // ADR-050 C3: write the rolling ChatMessage to liveBuffer.thinkingStream
    // so the v2 adapter can fold it into MessageBlock.  We construct a
    // synthetic ChatMessage that mirrors what the server would emit for
    // the completed thought (same id, joined content, same startTime).
    const lb = current.liveBuffer;
    if (!lb.thinkingStream || lb.thinkingStream.id !== msgId) {
      // First chunk of a new thought → edge-trigger the rolling entry.
      // startTime is stamped here (the authoritative moment the thought
      // began streaming) so downstream rendering can show a live duration
      // while the thought is in progress.  It is later read from the
      // liveBuffer entry by chatStore when record_complete lands.
      const draft: ChatMessage = {
        id: msgId,
        type: "thought",
        content: stream.lines.map((l) => l.content).join("\n"),
        timestamp: stream.startTime,
        startTime: stream.startTime,
      };
      patchSession(key, {
        liveBuffer: { ...lb, thinkingStream: draft },
        isThinking: true,
        thinkingStartTime: stream.startTime,
        thinkingContent: draft.content,
      });
      lastThinkingFlush.set(key, now);
    } else {
      // Throttled trailing preview flush — updates both liveBuffer and
      // the legacy text field at a bounded rate (500ms).  ADR-050:
      // no scroll-position gate; the v2 adapter's blocks are always
      // kept fresh regardless of viewport position.
      const last = lastThinkingFlush.get(key) ?? 0;
      if (now - last >= STREAM_FLUSH_THROTTLE_MS) {
        const content = stream.lines.map((l) => l.content).join("\n");
        if (content !== current.thinkingContent) {
          patchSession(key, {
            liveBuffer: { ...lb, thinkingStream: { ...lb.thinkingStream, content } },
            thinkingContent: content,
          });
        }
        lastThinkingFlush.set(key, now);
      }
    }
    emit({ kind: "liveUpdate", sessionKey: key });
    return;
  }

  // ── assistant ──
  let stream = activeStreams.get(key);
  const isFirstChunk = !stream || stream.messageId !== msgId;
  if (isFirstChunk) {
    stream = { messageId: msgId, role: "assistant", lineCount: 0, lines: [], startTime: now };
    activeStreams.set(key, stream);
    lastAssistantFlush.set(key, now);
  }
  if (!stream) return;
  stream.lineCount += lines.length;
  if (stream.lineCount > ASSISTANT_LINE_SAFETY_CAP) {
    // eslint-disable-next-line no-console
    console.warn(
      `[chatAdapterStore] safety valve: assistant activeStream hit ` +
        `${ASSISTANT_LINE_SAFETY_CAP}-line cap (messageId=${stream.messageId}). ` +
        `record_complete likely lost.`,
    );
    stream.lineCount = ASSISTANT_LINE_SAFETY_CAP;
  }
  for (const l of lines) {
    stream.lines.push({ role: "assistant", lineNo: l.line_no, content: l.content });
  }
  if (stream.lines.length > PREVIEW_LINE_CAP) {
    stream.lines = stream.lines.slice(-PREVIEW_LINE_CAP);
  }
  const current = readSession(key);
  const shouldBeReplying = stream.lineCount > ASSISTANT_REPLYING_LINE_THRESHOLD;
  const content = stream.lines.map((l) => l.content).join("\n");
  // ADR-050 C3: write the rolling ChatMessage to liveBuffer.assistantStream.
  const lb = current.liveBuffer;
  const newLbEntry: ChatMessage = isFirstChunk
    ? {
        id: msgId,
        type: "assistant",
        content,
        timestamp: stream.startTime,
      }
    : { ...(lb.assistantStream ?? { id: msgId, type: "assistant", timestamp: stream.startTime }), content };
  const patch: Partial<AdapterSessionState> = {
    liveBuffer: { ...lb, assistantStream: newLbEntry },
  };
  if (shouldBeReplying !== current.isAssistantReplying) {
    patch.isAssistantReplying = shouldBeReplying;
  }
  if (isFirstChunk) {
    patch.assistantStreamingStartTime = stream.startTime;
  }
  // Throttled trailing preview flush — updates the legacy text field
  // at a bounded rate (500ms).  ADR-050: no scroll-position gate;
  // liveBuffer is always kept fresh regardless of viewport position.
  {
    const last = lastAssistantFlush.get(key) ?? 0;
    if (now - last >= STREAM_FLUSH_THROTTLE_MS) {
      if (content !== current.assistantStreamingContent) {
        patch.assistantStreamingContent = content;
      }
      lastAssistantFlush.set(key, now);
    }
  }
  patchSession(key, patch);
  emit({ kind: "liveUpdate", sessionKey: key });
}

/**
 * Ingest a `record_complete` event.
 *
 * ADR-050 post-C5 fix: only clears the corresponding liveBuffer stream
 * (thinkingStream / assistantStream).  The completed record itself is
 * written directly into `messages[]` by chatStore.record_complete handler
 * (via `convertRecordCompleteToChatMessage`), so this function no longer
 * pushes anything into pendingRecordComplete (that field is removed).
 */
export function ingestRecordComplete(
  agentId: string,
  sessionId: string,
  args: { messageId: string; role: "assistant" | "thought" | "tool_call" | "tool_result" | string },
): void {
  const key = sessionKey(agentId, sessionId);
  const current = readSession(key);
  const lb = current.liveBuffer;
  const patch: Partial<AdapterSessionState> = {};

  // Clear the corresponding streaming preview.
  if (args.role === "thought" && lb.thinkingStream && lb.thinkingStream.id === args.messageId) {
    patch.liveBuffer = { ...lb, thinkingStream: null };
    if (current.isThinking) patch.isThinking = false;
    if (current.thinkingContent !== "") patch.thinkingContent = "";
    // Reset the legacy projection as well so it cannot leak into a
    // subsequent thought that arrives without stream_delta.
    patch.thinkingStartTime = null;
    lastThinkingFlush.delete(key);
  } else if (args.role === "assistant" && lb.assistantStream && lb.assistantStream.id === args.messageId) {
    patch.liveBuffer = { ...lb, assistantStream: null };
    if (current.isAssistantReplying) patch.isAssistantReplying = false;
    if (current.assistantStreamingContent !== "") patch.assistantStreamingContent = "";
    if (current.assistantStreamingStartTime !== null) patch.assistantStreamingStartTime = null;
    lastAssistantFlush.delete(key);
  }

  // Defensive: clear any stale streaming flags regardless of role.
  if (current.isAssistantReplying && !patch.isAssistantReplying) {
    patch.isAssistantReplying = false;
  }
  if (current.assistantStreamingContent !== "" && !("assistantStreamingContent" in patch)) {
    patch.assistantStreamingContent = "";
  }
  if (current.assistantStreamingStartTime !== null && !("assistantStreamingStartTime" in patch)) {
    patch.assistantStreamingStartTime = null;
    lastAssistantFlush.delete(key);
  }
  if (current.isThinking && args.role === "thought" && !("isThinking" in patch)) {
    patch.isThinking = false;
    if (current.thinkingContent !== "") patch.thinkingContent = "";
    lastThinkingFlush.delete(key);
  }

  if (Object.keys(patch).length > 0) {
    patchSession(key, patch);
  }
  const stream = activeStreams.get(key);
  if (stream && stream.messageId === args.messageId) {
    activeStreams.delete(key);
  }
  emit({ kind: "recordComplete", sessionKey: key });
}



/** Read a snapshot of a session's adapter state — used by chatStore to
 *  detect the "record_complete lost" edge case at idle. */
export function getChatAdapterSession(
  agentId: string,
  sessionId: string,
): AdapterSessionState {
  return readSession(sessionKey(agentId, sessionId));
}

/** Read the liveBuffer for a single (agent, session).  Used by the v2
 *  adapter's blocksSelector. */
export function getLiveBuffer(agentId: string, sessionId: string): LiveBuffer {
  return readSession(sessionKey(agentId, sessionId)).liveBuffer;
}

// ── Adapter ingest API (C3 additions) ─────────────────────────────────────
//
// `ingestSessionMessagesWindow` is the v2 adapter's bridge from the HTTP
// layer to the liveBuffer.  When an HTTP response lands, the v2 adapter
// tells chatAdapterStore which ids the server confirmed so the matching
// liveBuffer entries (pendingUserMessage / pendingRecordComplete[]) can
// be cleared.  It also emits a `pageLoaded` event for downstream
// subscribers (scrollController / ChatPanel).

export function ingestSessionMessagesWindow(
  agentId: string,
  sessionId: string,
  confirmedIds: ReadonlySet<string>,
  meta: { direction: "prev" | "next" | "initial"; offset: number; limit: number; total: number },
): void {
  const key = sessionKey(agentId, sessionId);
  // Clear any pending entries the server confirmed.
  if (confirmedIds.size > 0) {
    clearOptimisticEntries(agentId, sessionId, confirmedIds);
  }
  emit({ kind: "pageLoaded", sessionKey: key });
  // The direction/offset/limit/total is exposed via the event for
  // diagnostic consumers (C4 scrollController).  v2 adapter itself
  // already has these in chatStore; this event is for downstream
  // listeners that don't read chatStore directly.
  // We attach a no-op extension: subscribers can read the latest
  // chatStore cursor themselves.
  void meta;
}

/** Release all per-session state (used on session close / eviction). */
export function releaseAdapterSession(agentId: string, sessionId: string): void {
  const key = sessionKey(agentId, sessionId);
  activeStreams.delete(key);
  lastThinkingFlush.delete(key);
  lastAssistantFlush.delete(key);
  useChatAdapterStore.setState((state) => {
    if (!(key in state.sessions)) return state;
    const { [key]: _drop, ...rest } = state.sessions;
    return { ...state, sessions: rest };
  });
}

// ── Hooks for legacy consumers ─────────────────────────────────────────────
//
// C2 keeps the per-field React shape so ChatPanel / VML / ExploreBlock
// can be migrated incrementally (one subscription at a time).  These
// hooks return the *current* projection of the adapter state for a given
// session key.  C3 will replace them with adapter-block reads.

export interface LiveStateForConsumer {
  isThinking: boolean;
  thinkingStartTime: number | null;
  thinkingContent: string;
  assistantStreamingContent: string;
  assistantStreamingStartTime: number | null;
  isAssistantReplying: boolean;
  optimisticEntries: ChatMessage[];
}

/**
 * Subscribe to the live-stream state for a single (agent, session).
 * Returns the same shape every consumer used to read off chatStore
 * before ADR-050 — the per-field projection is preserved verbatim so
 * the consumer migration is mechanical.
 */
export function useLiveStream(
  agentId: string | null,
  sessionId: string | null,
): LiveStateForConsumer {
  const key = agentId && sessionId ? sessionKey(agentId, sessionId) : null;
  return useChatAdapterStore((s) => {
    if (!key) return DEFAULT_ADAPTER_SESSION_STATE;
    return s.sessions[key] ?? DEFAULT_ADAPTER_SESSION_STATE;
  });
}
