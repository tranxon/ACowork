/**
 * ADR-027: useStreamingContent — per-message streaming content via mutable store.
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
 *  3. GC pressure from per-poll object allocation is eliminated because
 *     streaming content updates never pass through Zustand's set().
 *
 * Returns `null` when the message is not a streaming message (or the
 * streaming entry hasn't been populated yet).
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
