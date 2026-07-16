import React, { useEffect, useLayoutEffect, useRef, useState } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import type { ChatMessage, ToolApprovalNeededEvent } from "../../lib/types";
import { AgentAvatar } from "../common/AgentAvatar";
import { ExploreBlock } from "./ExploreBlock";
import { MessageBubble } from "./MessageBubble";
import type { SessionScope } from "./useSessionScope";
import type { MessageBlock } from "./ChatPanel";
import { estimateBlockHeight } from "./blockHeightEstimator";

// ── Types ────────────────────────────────────────────────────────────

interface VirtualMessageListProps {
  /**
   * Pre-folded display blocks derived from raw `ChatMessage[]` by
   * ChatPanel.  Every block carries `anchorToLatest` (data semantics:
   * "this block contains the latest raw entry") and optional
   * `anchorToUser` (transient: set on one block per "load older" cycle).
   * The rendering layer reads ONLY from this array — never from raw
   * messages — to keep the data/UI boundary strict.
   */
  messageBlocks: MessageBlock[];
  /** Total virtual item count (messages + extra items like compacting indicator). */
  virtualCount: number;
  /** Whether to show the compacting indicator as an extra virtual item. */
  showCompactingItem: boolean;
  /** Whether the session is currently streaming. */
  sending: boolean;
  /** Pending tool approvals keyed by tool_call_id. */
  pendingApproval: Record<string, ToolApprovalNeededEvent>;
  /** Current session ID (for ExploreBlock). */
  currentSessionId: string | null;
  /** Current agent ID (for AgentAvatar). */
  selectedAgentId: string | null;
  /** Agent display name (for AgentAvatar). */
  agentDisplayName: string | undefined;
  /** Agent metadata (for AgentAvatar). */
  selectedAgent: {
    avatar?: string | null;
    version?: string;
    builtin_avatar?: string | null;
    role?: string;
  } | undefined;
  /** User display name (for MessageBubble). */
  userDisplayName: string;
  /** User avatar URL (for MessageBubble). */
  userAvatarUrl: string | null | undefined;
  /** User builtin avatar ID (for MessageBubble). */
  userBuiltinAvatarId: string | null | undefined;
  /** Tool approval callback. */
  onApprove: (action: "allow" | "deny", approval: ToolApprovalNeededEvent) => void;
  /** Translation function. */
  t: (key: string, params?: Record<string, unknown>) => string;
  /** Ref to the scroll container (owned by ChatPanel). */
  scrollContainerRef: React.RefObject<HTMLDivElement | null>;
  /** Per-session scope ref (for isLoadingMore, anchorToUserBlockId, etc.). */
  scope: React.MutableRefObject<SessionScope>;
  /**
   * Ref to the "pinned-to-bottom" flag — a per-USER-INTENT UI preference
   * (not per-session).  Owned by ChatPanel so it survives useSessionScope's
   * session-change reset.  VirtualMessageList writes to it from its mount
   * effect and reads it from the sticky-bottom effect.
   */
  pinnedToBottomRef: React.MutableRefObject<boolean>;
  /** Whether "load more" is in progress. */
  isLoadingMore: boolean;
  /**
   * True iff older data exists beyond the current cache window
   * (`messageOffset + messageLimit < messageTotal`).  Used by the
   * ensureRenderable effect to decide whether to invoke `onNeedMore`
   * when the rendered viewport is shorter than the container.
   */
  hasOlder: boolean;
  /**
   * Called by the ensureRenderable effect when the rendered viewport is
   * shorter than the container AND `hasOlder` is true.  Wired to
   * `chatStore.loadMoreOlderMessages` by the parent (ChatPanel).  Fires
   * one page at a time; the effect re-checks after the load completes
   * and may fire again until the viewport is full (or we hit the top).
   */
  onNeedMore: () => void;
  /** Whether the session is loading. */
  isLoadingSession: boolean;
  /** Session load error message. */
  loadError: string | null;
  /** Raw messages array (for empty-state check). */
  messages: ChatMessage[];
  /** If provided, restore this scroll offset on mount (nav-back from Settings). */
  initialScrollOffset?: number;
  /** Called when user clicks retry after session load failure. */
  onRetryLoadSession?: () => void;
}

// ── Imperative handle ─────────────────────────────────────────────────────
//
// Exposes pure data-derived queries about the current virtualized layout to
// the parent. Every method returns information that is **strictly derivable
// from the rendered MessageBlock array and the virtualizer's measured
// layout** — no scrollTop pixel guessing, no estimateSize-based heuristics.
//
// Used by ChatPanel to:
//   - pick the "anchorToUser" block before triggering loadMoreOlderMessages
//   - compute `pinnedToBottom` for nav-back snapshots without trusting
//     scrollTop pixel arithmetic.

export interface VirtualMessageListHandle {
  /**
   * Index (into the messageBlocks array) of the first block currently
   * visible in the viewport, or null if the virtualizer has no measured
   * layout yet (e.g. immediately after mount).
   */
  getFirstVisibleBlockIndex: () => number | null;
  /**
   * Index of the last block currently visible in the viewport, or null.
   */
  getLastVisibleBlockIndex: () => number | null;
  /**
   * True iff at least one visible block carries `anchorToLatest === true`.
   * This is the strict, data-derived definition of "the user is at the
   * bottom of the conversation" — no scrollTop comparison, no threshold,
   * no estimateSize-derived assumptions.
   */
  isAnchorToLatestInView: () => boolean;
  /**
   * Scroll to the last virtual item, using virtualizer.scrollToIndex
   * (data-driven — the virtualizer computes the exact pixel offset from
   * measured item sizes).  Re-scrolls on the next animation frame so the
   * user lands on the *measured* bottom, not the estimateSize-derived
   * bottom (which undershoots when many items haven't been measured yet,
   * e.g. right after a load-older that prepended a long stretch of
   * un-rendered older blocks).  No-op if there are zero virtual items.
   */
  scrollToBottom: () => void;
}

// ── Component ───────────────────────────────────────────────────────

/**
 * Virtualized message list.
 *
 * IMPORTANT: This component MUST receive a `key` prop tied to the current
 * session (e.g. `key={currentScrollKey}`) from its parent.  The `key`
 * forces React to unmount and remount the entire component on session
 * switch, which creates a fresh `Virtualizer` instance with
 * `scrollOffset = 0`.  Without this, the Virtualizer instance persists
 * across sessions and its internal `scrollOffset` retains the previous
 * session's scroll position, causing `getVirtualItems()` to return an
 * empty array when the old offset exceeds the new session's total size.
 */
export const VirtualMessageList = React.forwardRef<
  VirtualMessageListHandle,
  VirtualMessageListProps
>(function VirtualMessageList(props, ref) {
  const {
    messageBlocks,
    virtualCount,
    showCompactingItem,
    sending,
    pendingApproval,
    currentSessionId,
    selectedAgentId,
    agentDisplayName,
    selectedAgent,
    userDisplayName,
    userAvatarUrl,
    userBuiltinAvatarId,
    onApprove,
    t,
    scrollContainerRef,
    scope,
    pinnedToBottomRef,
    isLoadingMore,
    hasOlder,
    onNeedMore,
    isLoadingSession,
    loadError,
    messages,
    initialScrollOffset,
    onRetryLoadSession,
  } = props;

  // ── Container width (self-measured) ─────────────────────────────
  //
  // VirtualMessageList owns its own width measurement via
  // scrollContainerRef.  The scroll container div exists unconditionally
  // in ChatPanel's JSX, so its ref is available in useLayoutEffect.
  //
  // React guarantees that setState inside useLayoutEffect is flushed
  // synchronously before the browser paints.  This means the init-scroll
  // effect (registered below) will skip on the first invocation
  // (containerWidth is still 0, didInitialScrollRef stays false), then
  // fire on the synchronous re-render (containerWidth > 0), all before
  // the first paint.  No flushSync, no rAF, no force-overscan.
  const [containerWidth, setContainerWidth] = useState(0);

  useLayoutEffect(() => {
    const el = scrollContainerRef.current;
    if (el && el.clientWidth > 0) {
      setContainerWidth((prev) => {
        const w = el.clientWidth;
        return prev === w ? prev : w;
      });
    }
  });

  // ResizeObserver for subsequent width changes (window resize, etc.)
  useEffect(() => {
    const el = scrollContainerRef.current;
    if (!el) return;
    const ro = new ResizeObserver((entries) => {
      for (const entry of entries) {
        const w = Math.round(entry.contentRect.width);
        setContainerWidth((prev) => (prev === w ? prev : w));
      }
    });
    ro.observe(el);
    return () => ro.disconnect();
  });

  // ── Virtualizer ──────────────────────────────────────────────────
  // Created fresh on every mount (thanks to parent's key={currentScrollKey}).
  // initialOffset: 0 ensures getScrollOffset() returns 0 on the first render,
  // before any scroll event has fired.
  //
  // estimateSize delegates to the data-driven estimator in
  // `blockHeightEstimator.ts`.  This eliminates the previous
  // force-overscan / double-rAF hack that tried to compensate for a
  // blanket 120px constant undershooting on long conversations.
  //
  // The estimator closes over `messageBlocks`, `containerWidth`, and
  // `showCompactingItem` — all of which are stable references per render
  // (messageBlocks is a memoized array, containerWidth is updated by a
  // ResizeObserver only on resize, and showCompactingItem is a primitive).
  // Because of this the estimator is a stable function reference; we
  // intentionally do NOT recreate it on every render, otherwise the
  // virtualizer's option-diffing would treat every render as a size
  // change and force a full re-layout.
  const estimateSize = React.useCallback(
    (index: number) =>
      estimateBlockHeight(index, messageBlocks, containerWidth, showCompactingItem),
    [messageBlocks, containerWidth, showCompactingItem],
  );

  const virtualizer = useVirtualizer({
    count: virtualCount,
    getScrollElement: () => scrollContainerRef.current,
    estimateSize,
    overscan: 5,
    gap: 4,
    initialOffset: initialScrollOffset ?? 0,
    // Custom scrollToFn: use synchronous scrollTop assignment instead of
    // element.scrollTo().  On WKWebView (macOS Safari), element.scrollTo()
    // can be asynchronous even with behavior:"auto", causing the scroll
    // event to fire after a delay.  Direct scrollTop assignment triggers
    // a synchronous scroll event in all browsers.
    //
    // Also handles the edge case where scrollTop doesn't change (e.g.
    // browser already clamped it to the target value).  In that case no
    // scroll event fires, so we toggle to force one — otherwise the
    // virtualizer's internal scrollOffset stays stale.
    scrollToFn: (offset, options, instance) => {
      const element = instance.scrollElement;
      if (!element) return;
      const target = offset + (options.adjustments ?? 0);
      if (options.behavior === "smooth") {
        element.scrollTo({
          [instance.options.horizontal ? "left" : "top"]: target,
          behavior: "smooth",
        });
      } else {
        const axis = instance.options.horizontal ? "scrollLeft" : "scrollTop";
        const current = element[axis] as number;
        element[axis] = target;
        // If the browser didn't actually change the scroll position
        // (e.g. it was already clamped to target), no scroll event fires.
        // Toggle to a different value and back to force one.
        if (element[axis] === current && target === current) {
          element[axis] = current + 1;
          element[axis] = target;
        }
      }
    },
  });

  // ── Imperative handle ─────────────────────────────────────
  // Expose three data-derived queries to the parent. None of them rely on
  // scrollTop pixel math or estimateSize-based heuristics; everything comes
  // from virtualizer.getVirtualItems() intersecting the messageBlocks array.
  //
  // These powers the "strict intermediate layer" architecture: ChatPanel
  // uses them to pick the anchorToUser block before triggering
  // loadMoreOlderMessages and to derive pinnedToBottom for nav-back
  // snapshots, without ever guessing at the user's reading position.
  React.useImperativeHandle(
    ref,
    () => ({
      getFirstVisibleBlockIndex: () => {
        const items = virtualizer.getVirtualItems();
        return items.length > 0 ? items[0].index : null;
      },
      getLastVisibleBlockIndex: () => {
        const items = virtualizer.getVirtualItems();
        return items.length > 0 ? items[items.length - 1].index : null;
      },
      isAnchorToLatestInView: () => {
        const items = virtualizer.getVirtualItems();
        return items.some((item) => {
          const block = messageBlocks[item.index];
          return block?.anchorToLatest === true;
        });
      },
      scrollToBottom: () => {
        const count = virtualizer.options.count;
        if (count === 0) return;
        virtualizer.scrollToIndex(count - 1, { align: "end" });
      },
    }),
    [virtualizer, messageBlocks],
  );

  // ── Initialization: scroll to bottom on first data arrival ───────
  //
  // Uses useLayoutEffect so scroll offset is set before the first paint
  // (no flash of top position).
  //
  // containerWidth is measured synchronously (useLayoutEffect + flushSync)
  // by the width-measurement effect above, so by the time this effect fires
  // with containerWidth > 0, estimateSize produces accurate per-block
  // heights — scrollToIndex(end) lands on the real bottom on the first
  // call.  Same code path as the arrow button's scrollToBottom().
  //
  // No force-overscan, no rAF.
  //
  // didInitialScrollRef gates against re-initialization (e.g. prepends from
  // ensureRenderable).  The parent's key={currentScrollKey} resets the ref
  // on every session switch.
  const didInitialScrollRef = useRef(false);
  useLayoutEffect(() => {
    if (didInitialScrollRef.current) return;
    if (virtualCount === 0) return; // wait for data
    if (containerWidth <= 0) return; // wait for ResizeObserver measurement

    const container = scrollContainerRef.current;
    if (!container) return;

    didInitialScrollRef.current = true;

    if (initialScrollOffset !== undefined && initialScrollOffset >= 0) {
      container.scrollTop = initialScrollOffset;
      pinnedToBottomRef.current = false;
      return;
    }

    pinnedToBottomRef.current = true;
    virtualizer.scrollToIndex(virtualCount - 1, { align: "end" });
  }, [virtualCount, initialScrollOffset, containerWidth]); // eslint-disable-line react-hooks/exhaustive-deps

  // ── Load-OLDER: scroll to anchor block after messages prepended ───────
  //
  // When the user scrolls near the top and `loadMoreOlderMessages` fires,
  // ChatPanel has already recorded the first visible block's `blockId` into
  // `scope.current.anchorToUserBlockId` BEFORE the HTTP request.  After
  // the older messages prepend and `isLoadingMore` flips back to false,
  // `messageBlocks` is rebuilt with the new prepended block(s); exactly one
  // of those blocks now carries `anchorToUser: true` (set by the
  // messageBlocks useMemo, which reads the scope ref).  We scroll to that
  // block via `virtualizer.scrollToIndex` — a data-driven scroll, not a
  // scrollTop-pixel-math guess — and immediately clear the field so it
  // doesn't fire twice.
  //
  // SCROLL-DOWN / "go to latest" needs NO restoration here: under the
  // unified data-window model, the cache starts at offset=0 (the most
  // recent MESSAGE_CACHE_WINDOW entries) via ensureLatestInCache, so the
  // user always has the tail already in the cache when they navigate to
  // the bottom.  scrollToBottom in ChatPanel just re-issues the same
  // ensureLatestInCache + a single scrollToIndex(end), no prepend
  // restoration needed.
  const prevIsLoadingMoreRef = useRef(false);
  useLayoutEffect(() => {
    const wasLoading = prevIsLoadingMoreRef.current;
    prevIsLoadingMoreRef.current = isLoadingMore;
    if (!wasLoading || isLoadingMore) return;
    const anchorBlockId = scope.current.anchorToUserBlockId;
    if (!anchorBlockId) return;
    const idx = messageBlocks.findIndex((b) => b.blockId === anchorBlockId);
    if (idx < 0) return;
    scope.current.anchorToUserBlockId = null;
    virtualizer.scrollToIndex(idx, { align: "start" });
  }, [isLoadingMore, virtualizer, scope, messageBlocks]);

  // ── Sticky-bottom: keep pinned when new items are appended ─────────
  //
  // Trigger condition is purely data-derived:
  //   - `virtualCount` increased (a new block was APPENDED to messageBlocks,
  //     i.e. rawCount grew in tail position; prepends also grow virtualCount
  //     but those don't trigger because the load-older effect above just
  //     scrolled to the anchorToUser block, putting the user away from
  //     the bottom by definition)
  //   - `pinnedToBottomRef.current === true` (per-USER-INTENT flag, owned
  //     by ChatPanel; the only thing that flips it is a real user scroll)
  //
  // When a streaming block grows in place (same block, more content),
  // virtualCount doesn't change, but the viewport DOES contain an
  // anchorToLatest block — and the virtualizer's measureElement ref
  // resizes the row, which on its own re-renders the viewport with the
  // anchorToLatest block still visible.  The `isAnchorToLatestInView`
  // query via the imperative handle detects that case and re-anchors
  // to the bottom of the now-taller block.
  const prevVirtualCountRef = useRef(0);
  useLayoutEffect(() => {
    if (!didInitialScrollRef.current) return;
    const countGrew = virtualCount > prevVirtualCountRef.current;
    prevVirtualCountRef.current = virtualCount;
    if (!countGrew) return;
    if (!pinnedToBottomRef.current) return;
    if (virtualCount === 0) return;
    virtualizer.scrollToIndex(virtualCount - 1, { align: "end" });
  }, [virtualCount, virtualizer, pinnedToBottomRef]);

  // ── Ensure-renderable: load more older pages until viewport is full ────────
  //
  // The unified data-window model: the cache starts at offset=0 holding the
  // LAST MESSAGE_CACHE_WINDOW raw entries (via ensureLatestInCache).  If those
  // entries don't yet fill the viewport — e.g. a brand-new session with very
  // few messages, or a session whose blockHeight average is unusually tall —
  // we need to prepend older pages until the rendered content overflows the
  // container, or we hit the top of the conversation (`!hasOlder`).
  //
  // "Filled" is judged strictly by measurementsCache (the virtualizer's real,
  // measured heights) compared against the container's clientHeight.  No
  // estimateSize-based guesswork, no scrollTop pixel arithmetic.
  //
  // The effect re-runs on every virtualCount / virtualizer / hasOlder /
  // isLoadingMore change so it keeps making progress after each page lands.
  // The parent's guard inside loadMoreOlderMessages (`if (isLoadingMore) return;`)
  // prevents overlapping requests — this effect just keeps firing until either
  // the viewport fills or we run out of older pages.
  //
  // Hard cap: at most MAX_ENSURE_RENDERABLE_PAGES consecutive page-loads per
  // component mount.  Protects against pathological layouts (e.g. an extra-
  // small viewport combined with unusually tall per-block heights) where
  // legitimately filling the viewport would require dozens of HTTP requests
  // and thousands of raw entries.  After the cap is hit the user sees what
  // they have — the next upward scroll naturally triggers more
  // loadMoreOlderMessages through the normal handleScroll path.  The counter
  // resets on every session switch because the parent's key={currentScrollKey}
  // remounts this component.
  const MAX_ENSURE_RENDERABLE_PAGES = 10;
  const ensureRenderableCountRef = useRef(0);
  useLayoutEffect(() => {
    if (!didInitialScrollRef.current) return;
    if (virtualCount === 0) return;
    if (isLoadingMore) return;
    if (!hasOlder) return;
    if (ensureRenderableCountRef.current >= MAX_ENSURE_RENDERABLE_PAGES) return;

    const container = scrollContainerRef.current;
    if (!container) return;

    // virtualizer.getTotalSize() walks the measurementsCache; if a block has
    // never been rendered, its estimateSize fills the gap.  The container's
    // clientHeight is the actual viewport height.  If the real rendered total
    // is below the viewport, we still need more.
    const totalHeight = virtualizer.getTotalSize();
    const viewportHeight = container.clientHeight;
    if (totalHeight >= viewportHeight) return;

    ensureRenderableCountRef.current += 1;
    onNeedMore();
  }, [
    virtualCount,
    virtualizer,
    hasOlder,
    isLoadingMore,
    onNeedMore,
    scrollContainerRef,
  ]);

  // ── Render ───────────────────────────────────────────────────────
  return (
    <>
      {/* Loading more indicator at top */}
      {isLoadingMore && (
        <div className="flex items-center justify-center py-2">
          <span className="inline-block h-4 w-4 animate-spin rounded-full border-2 border-zinc-300 border-t-zinc-600 dark:border-zinc-600 dark:border-t-zinc-300" />
          <span className="ml-1.5 text-[10px] text-zinc-400 dark:text-zinc-500">Loading more...</span>
        </div>
      )}

      {/* Loading session indicator */}
      {isLoadingSession && messages.length === 0 && (
        <div className="flex h-full items-center justify-center">
          <div className="text-center">
            <span className="inline-block h-8 w-8 animate-spin rounded-full border-2 border-zinc-300 border-t-zinc-600 dark:border-zinc-600 dark:border-t-zinc-300" />
            <p className="mt-3 text-xs text-zinc-400 dark:text-zinc-500">Loading conversation...</p>
          </div>
        </div>
      )}

      {loadError && !isLoadingSession && (
        <div className="flex h-full flex-col items-center justify-center gap-3 px-4">
          <div className="max-w-md rounded-md border border-red-200 bg-red-50 p-3 text-xs text-red-700 dark:border-red-900 dark:bg-red-950 dark:text-red-300">
            {t("chatPanel.sessionLoadFailed")}
            {loadError && <div className="mt-1 text-red-500 dark:text-red-400">{loadError}</div>}
          </div>
          <button
            onClick={() => onRetryLoadSession?.()}
            className="rounded-md bg-zinc-100 px-3 py-1.5 text-xs text-zinc-700 hover:bg-zinc-200 dark:bg-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-600"
          >
            {t("chatPanel.retry")}
          </button>
        </div>
      )}

      {!loadError && !isLoadingSession && messages.length === 0 && (
        <div className="flex h-full items-center justify-center text-xs text-zinc-400 dark:text-zinc-500">
          Start a conversation
        </div>
      )}

      {/* Virtualized message list */}
      {messageBlocks.length > 0 && (
        <div
          style={{
            height: virtualizer.getTotalSize(),
            width: "100%",
            position: "relative",
          }}
        >
          {virtualizer.getVirtualItems().map((virtualRow) => {
            // Compacting indicator is the only extra virtual item.
            const compactingIdx = messageBlocks.length;

            // --- Compacting indicator (extra virtual item) ---
            if (showCompactingItem && virtualRow.index === compactingIdx) {
              return (
                <div
                  key={virtualRow.key}
                  ref={virtualizer.measureElement}
                  data-index={virtualRow.index}
                  style={{
                    position: "absolute",
                    top: 0,
                    left: 0,
                    width: "100%",
                    transform: `translateY(${virtualRow.start}px)`,
                  }}
                >
                  <div className="flex items-center gap-1.5 ml-12 py-1.5 select-none">
                    <span className="shrink-0 h-1.5 w-1.5 rounded-full bg-[var(--color-accent)] animate-pulse" />
                    <span className="thinking-shimmer" style={{ fontSize: "var(--ui-font-size, 0.875rem)" }}>{t("chatPanel.compacting")}</span>
                  </div>
                </div>
              );
            }

            // --- Regular message item ---
            const item = messageBlocks[virtualRow.index];

            return (
              <div
                key={virtualRow.key}
                ref={virtualizer.measureElement}
                data-index={virtualRow.index}
                style={{
                  position: "absolute",
                  top: 0,
                  left: 0,
                  width: "100%",
                  transform: `translateY(${virtualRow.start}px)`,
                }}
              >
                {/* Agent header — shown before first agent message after a user message. */}
                {(() => {
                  let isPrevUser = false;
                  for (let i = virtualRow.index - 1; i >= 0; i--) {
                    const prev = messageBlocks[i];
                    const prevType = prev.type;
                    if (prevType === "user") {
                      isPrevUser = true;
                      break;
                    }
                    if (
                      prevType === "compaction" ||
                      prevType === "system" ||
                      prevType === "document_upload"
                    ) {
                      continue;
                    }
                    break;
                  }
                  if (!isPrevUser) return null;
                  const t = item.type;
                  const isAgent = t === "explore_group"
                    || (t !== "user"
                      && t !== "system"
                      && t !== "compaction"
                      && t !== "document_upload");
                  if (!isAgent) return null;
                  return (
                    <div className="flex items-center gap-2 mb-2 mt-1">
                      <AgentAvatar
                        agentId={selectedAgentId ?? ""}
                        displayName={agentDisplayName}
                        avatarUrl={selectedAgent?.avatar}
                        version={selectedAgent?.version}
                        builtinAvatarId={selectedAgent?.builtin_avatar ?? null}
                        size={40}
                        className="shrink-0"
                      />
                      <div className="flex flex-col">
                        <span className="text-xs font-medium text-zinc-500 dark:text-zinc-400">
                          {agentDisplayName}
                        </span>
                        {selectedAgent?.role && (
                          <span className="text-[10px] leading-tight text-zinc-400 dark:text-zinc-500">
                            {selectedAgent.role}
                          </span>
                        )}
                      </div>
                    </div>
                  );
                })()}

                {/* Explore group - aggregated think + tool calls/results */}
                {item.type === "explore_group" && (() => {
                  const nextItem = messageBlocks[virtualRow.index + 1];
                  const hasFollowUpReply = nextItem !== undefined && nextItem.type !== "explore_group";
                  const isLastGroup = virtualRow.index === messageBlocks.length - 1;
                  const isStreamingGroup = sending && isLastGroup
                    && item.items.some((it: ChatMessage) => it.isStreaming === true);
                  return (
                    <div className="ml-12">
                      <ExploreBlock
                        items={item.items}
                        isStreaming={isStreamingGroup}
                        pendingApproval={pendingApproval}
                        currentSessionId={currentSessionId}
                        onApprove={(action, approval) => onApprove(action, approval)}
                        hasFollowUpReply={hasFollowUpReply}
                      />
                    </div>
                  );
                })()}

                {/* Regular message */}
                {item.type !== "explore_group" && (() => {
                  const msg = item.items[0];
                  return (
                    <MessageBubble
                      message={msg}
                      currentSessionId={currentSessionId ?? ""}
                      liveUserName={userDisplayName}
                      liveUserAvatarUrl={userAvatarUrl}
                      liveUserBuiltinAvatarId={userBuiltinAvatarId}
                    />
                  );
                })()}
              </div>
            );
          })}
        </div>
      )}

      {/* Scroll-to-bottom button is rendered by ChatPanel (outside the scroll
          container) so it stays fixed in view.  VirtualMessageList lives
          inside overflow-y-auto and would scroll the button with content. */}
    </>
  );
});
