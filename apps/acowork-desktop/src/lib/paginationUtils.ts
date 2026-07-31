/**
 * Pagination utility functions shared between chatStore and chatListAdapter.
 *
 * ADR-050: The forward (oldest-end) offset model means `offset=0` is the
 * oldest entry and `offset + limit >= total` is the newest (tail).
 */

/**
 * Determine whether the current cache window covers the tail of the
 * conversation.  When true, real-time data (MQTT stream_delta previews
 * and record_complete writes) should be injected into the rendering
 * pipeline.
 *
 * `limit === 0` (fresh session, no HTTP load yet) is considered at-tail
 * so that liveBuffer streaming previews and record_complete writes are
 * active immediately - the user must see their optimistic message and
 * any streaming preview even before the initial HTTP load completes.
 *
 * NOTE: This function answers "should real-time data be injected?".
 * It is NOT the right check for "should we skip loading the tail page?"
 * - use `messageLimit > 0 && ...` for that (scrollToBottom /
 *   ensureLatestInCache), because `limit === 0` means no page is loaded
 *   and an HTTP request IS needed.
 */
export function isAtTail(
  offset: number,
  limit: number,
  total: number,
): boolean {
  return limit === 0 || offset + limit >= total;
}
