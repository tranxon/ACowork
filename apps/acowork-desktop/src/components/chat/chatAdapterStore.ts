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

/** Per-session state owned by chatAdapterStore.  C2 keeps the legacy
 *  field shape verbatim so consumers can be migrated one-by-one. */
export interface AdapterSessionState {
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
   */
  optimisticEntries: ChatMessage[];
  /** Pinned-to-bottom signal — set by the scroll controller.  C2 keeps
   *  the field here so existing consumers don't break; C4 will move
   *  the read/write to the scroll controller's own subscription model. */
  isPinnedToBottom: boolean;
}

const DEFAULT_ADAPTER_SESSION_STATE: AdapterSessionState = {
  isThinking: false,
  thinkingStartTime: null,
  thinkingContent: "",
  assistantStreamingContent: "",
  assistantStreamingStartTime: null,
  isAssistantReplying: false,
  optimisticEntries: [],
  isPinnedToBottom: true,
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
 * Called from chatStore.sendMessage after the MQTT publish succeeds.
 * The overlay is consumed by the v2 adapter (C3) which sorts it into
 * display position; the legacy path (ChatPanel) reads it directly via
 * a subscription so the message is visible immediately.
 */
export function ingestOptimisticUserMessage(
  agentId: string,
  sessionId: string,
  entries: ChatMessage[],
): void {
  const key = sessionKey(agentId, sessionId);
  const current = readSession(key);
  patchSession(key, {
    optimisticEntries: [...current.optimisticEntries, ...entries],
  });
  emit({ kind: "flushAvailable", sessionKey: key });
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
  patchSession(key, { optimisticEntries: remaining });
}

/** Drop every optimistic entry for a session.  Used on session close. */
export function clearAllOptimisticEntries(agentId: string, sessionId: string): void {
  const key = sessionKey(agentId, sessionId);
  const current = readSession(key);
  if (current.optimisticEntries.length === 0) return;
  patchSession(key, { optimisticEntries: [] });
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

  if (role === "thought") {
    const msgId = lines[0].message_id;
    let stream = activeStreams.get(key);
    if (!stream || stream.messageId !== msgId) {
      stream = { messageId: msgId, role: "thought", lineCount: 0, lines: [], startTime: Date.now() };
      activeStreams.set(key, stream);
    }
    for (const l of lines) {
      stream.lines.push({ role: "thought", lineNo: l.line_no, content: l.content });
    }
    if (stream.lines.length > PREVIEW_LINE_CAP) {
      stream.lines = stream.lines.slice(-PREVIEW_LINE_CAP);
    }
    const current = readSession(key);
    if (!current.isThinking) {
      // Edge-trigger: first chunk of a new thought → flip on + seed.
      patchSession(key, {
        isThinking: true,
        thinkingStartTime: stream.startTime,
        thinkingContent: stream.lines.map((l) => l.content).join("\n"),
      });
      lastThinkingFlush.set(key, Date.now());
    } else if (current.isPinnedToBottom) {
      // Throttled trailing preview flush — only when user is at the
      // bottom (the ThinkBlock is in the viewport).  Mirrors the
      // pre-ADR-050 store's "skip flush when not at bottom" rule.
      const now = Date.now();
      const last = lastThinkingFlush.get(key) ?? 0;
      if (now - last >= STREAM_FLUSH_THROTTLE_MS) {
        const content = stream.lines.map((l) => l.content).join("\n");
        if (content !== current.thinkingContent) {
          patchSession(key, { thinkingContent: content });
        }
        lastThinkingFlush.set(key, now);
      }
    }
    emit({ kind: "liveUpdate", sessionKey: key });
    return;
  }

  // ── assistant ──
  const msgId = lines[0].message_id;
  let stream = activeStreams.get(key);
  const isFirstChunk = !stream || stream.messageId !== msgId;
  if (isFirstChunk) {
    stream = { messageId: msgId, role: "assistant", lineCount: 0, lines: [], startTime: Date.now() };
    activeStreams.set(key, stream);
    lastAssistantFlush.set(key, Date.now());
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
  if (shouldBeReplying !== current.isAssistantReplying) {
    patchSession(key, { isAssistantReplying: shouldBeReplying });
  }
  if (isFirstChunk) {
    patchSession(key, { assistantStreamingStartTime: stream.startTime });
  }
  if (current.isPinnedToBottom) {
    const now = Date.now();
    const last = lastAssistantFlush.get(key) ?? 0;
    if (now - last >= STREAM_FLUSH_THROTTLE_MS) {
      const content = stream.lines.map((l) => l.content).join("\n");
      if (content !== current.assistantStreamingContent) {
        patchSession(key, { assistantStreamingContent: content });
      }
      lastAssistantFlush.set(key, now);
    }
  }
  emit({ kind: "liveUpdate", sessionKey: key });
}

/**
 * Ingest a `record_complete` event.  Clears the per-stream active
 * tracker, resets the trailing preview flags, and notifies subscribers.
 * The pre-ADR-050 chatStore also called `scheduleRefresh` here — that
 * responsibility moves to the adapter (C3 will turn it into a
 * `flushAvailable` subscriber that triggers an HTTP refresh).
 */
export function ingestRecordComplete(
  agentId: string,
  sessionId: string,
  args: { messageId: string; role: "assistant" | "thought" | "tool_call" | "tool_result" | string },
): void {
  const key = sessionKey(agentId, sessionId);
  const current = readSession(key);
  const patch: Partial<AdapterSessionState> = {};
  if (current.isAssistantReplying) {
    patch.isAssistantReplying = false;
  }
  if (current.assistantStreamingContent !== "" || current.assistantStreamingStartTime !== null) {
    patch.assistantStreamingContent = "";
    patch.assistantStreamingStartTime = null;
    lastAssistantFlush.delete(key);
  }
  if (args.role === "thought" && current.isThinking) {
    patch.isThinking = false;
    patch.thinkingContent = "";
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
  // C3 will also wire flushAvailable → HTTP refresh here.
}

/**
 * Update the pinned-to-bottom signal.  Owned by the scroll controller
 * (C4 will turn the controller into an event-driven subscription that
 * writes this field through this single function).
 */
export function setPinnedToBottom(
  agentId: string,
  sessionId: string,
  value: boolean,
): void {
  const key = sessionKey(agentId, sessionId);
  const current = readSession(key);
  if (current.isPinnedToBottom === value) return;
  patchSession(key, { isPinnedToBottom: value });
}

/** Read the pinned-to-bottom signal.  Used by the scroll controller. */
export function isPinnedToBottom(agentId: string, sessionId: string): boolean {
  return readSession(sessionKey(agentId, sessionId)).isPinnedToBottom;
}

/** Read a snapshot of a session's adapter state — used by chatStore to
 *  detect the "record_complete lost" edge case at idle. */
export function getChatAdapterSession(
  agentId: string,
  sessionId: string,
): AdapterSessionState {
  return readSession(sessionKey(agentId, sessionId));
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
  isPinnedToBottom: boolean;
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
