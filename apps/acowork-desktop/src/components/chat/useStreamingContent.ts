/**
 * ADR-035: useStreamingContent — per-message streaming content via the
 * per-session `activeStreams` Map (replaces the ADR-027 `streamingContents`
 * multi-buffer with a single per-session active buffer).
 *
 * Reads the volatile `content` and `isStreaming` flag for a single streaming
 * message from the external mutable Map (not React state).  Uses
 * `useSyncExternalStore` so that:
 *
 *  1. Only the MessageBubble that owns this message re-renders when its
 *     content changes — siblings are untouched.
 *  2. The ChatMessage object reference in React state is never mutated —
 *     React.memo comparison (prev.message === next.message) still correctly
 *     skips re-renders for settled messages.
 *  3. GC pressure from per-push object allocation is eliminated because
 *     streaming content updates never pass through Zustand's set().
 *
 * Returns `null` when the message is not a streaming message (or the
 * streaming entry hasn't been populated yet).
 *
 * Note: ADR-035 impact list mentions deleting this file, but it is retained
 * as a thin wrapper over `getStreamingContent`/`subscribeStreaming` to avoid
 * duplicating `useSyncExternalStore` boilerplate in MessageBubble/ExploreBlock.
 * The underlying data source changed from ADR-027's multi-buffer Map to
 * ADR-035's per-session single buffer — the hook's contract is unchanged.
 */
import { useSyncExternalStore } from "react";
import { getStreamingContent, subscribeStreaming } from "../../stores/chatStore";

interface StreamingContent {
  /** Full accumulated content (replaces message.content in the renderer). */
  content: string;
  /** Whether the message is still being streamed (controls pulse cursor). */
  isStreaming: boolean;
}

/**
 * Subscribe to streaming content for a single message.
 *
 * @param sessionId - Current session ID
 * @param messageId - ChatMessage.id (e.g. "streaming:42")
 * @returns { content, isStreaming } or null if this message isn't streaming
 */
export function useStreamingContent(
  sessionId: string,
  messageId: string,
): StreamingContent | null {
  return useSyncExternalStore(
    (callback) => subscribeStreaming(sessionId, messageId, callback),
    () => getStreamingContent(sessionId, messageId),
  );
}
