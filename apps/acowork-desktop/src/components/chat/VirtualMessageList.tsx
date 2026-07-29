import React, { useEffect, useLayoutEffect, useRef, useState, useCallback } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import type { ChatMessage, ToolApprovalNeededEvent } from "../../lib/types";
import { AgentAvatar } from "../common/AgentAvatar";
import { ExploreBlock } from "./ExploreBlock";
import { MessageBubble } from "./MessageBubble";
import { UserWithAttachmentsBubble } from "./UserWithAttachmentsBubble";
import type { MessageBlock } from "./messageFolder";
import type { ChatListAdapter } from "./useChatListAdapter";
import { estimateBlockHeight, recordMeasuredHeight } from "./blockHeightEstimator";
import { StreamingSourceBlock } from "./StreamingSourceBlock";

// ResizeObserver instances per element.  WeakMap so they're GC'd when the
// element is removed from the DOM (virtual list recycling).
const resizeObservers = new WeakMap<HTMLElement, ResizeObserver>();

// ── Types ────────────────────────────────────────────────────────────

interface VirtualMessageListProps {
  /**
   * ADR-041 C4: The ChatListAdapter - single bridge between chatStore
   * and VML. Provides blocks, pagination state/actions, scroll anchoring,
   * sticky-bottom, and ensure-renderable (onLayout).
   */
  adapter: ChatListAdapter;
  /**
   * Pre-folded display blocks from the adapter (adapter.blocks).
   * Kept as a separate prop for direct access in the virtualizer's
   * estimateSize and render functions.
   */
  messageBlocks: MessageBlock[];
  /**
   * Total virtual item count, owned by ChatPanel - already includes the
   * trailing extras (compacting indicator and/or replying indicator).
   */
  virtualCount: number;
  /** Whether to show the compacting indicator as an extra virtual item. */
  showCompactingItem: boolean;
  /** Whether to show the replying indicator as an extra virtual item. */
  showReplyingItem: boolean;
  /** Whether the session is currently streaming. */
  sending: boolean;
  pendingApproval: Record<string, ToolApprovalNeededEvent>;
  currentSessionId: string | null;
  /** ADR-045: Per-tool progress heartbeat, keyed by tool_call_id. */
  toolProgress?: Record<string, { elapsedMs: number; timeoutMs: number }>;
  /** Live thinking state from the store, propagated to the last explore
   *  group so it can render a real-time ThinkBlock inside its expanded
   *  area before the HTTP refresh lands. */
  isThinking: boolean;
  thinkingContent: string;
  thinkingStartTime: number | null;
  /** Live assistant streaming text (last 5 lines, joined). Rendered
   *  inside the trailing replying slot via StreamingSourceBlock.
   *  Mirrors the `thinkingContent` pattern: throttled flush from the
   *  store, cleared on record_complete. */
  assistantStreamingContent: string;
  /** When the current assistant stream started — used by the trailing
   *  StreamingSourceBlock's duration timer. */
  assistantStreamingStartTime: number | null;
  /** ADR-045: Cancel a single in-flight tool execution. */
  onCancelTool?: (toolCallId: string) => void;
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
// Used by ScrollController (in ChatPanel) to:
//   - scroll to top/bottom via scrollToTop/scrollToBottom (used for
//     init scroll, sticky-bottom, and jump target).

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
   * Scroll to the last virtual item, using virtualizer.scrollToIndex
   * (data-driven).  No-op if there are zero virtual items.
   */
  scrollToBottom: () => void;
  /**
   * Scroll to the first virtual item, using virtualizer.scrollToIndex
   * with align: "start".  No-op if there are zero virtual items.
   */
  scrollToTop: () => void;
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
    adapter,
    messageBlocks,
    virtualCount,
    showCompactingItem,
    showReplyingItem,
    sending,
    pendingApproval,
    currentSessionId,
    toolProgress,
    onCancelTool,
    isThinking,
    thinkingContent,
    thinkingStartTime,
    assistantStreamingContent,
    assistantStreamingStartTime,
    selectedAgentId,
    agentDisplayName,
    selectedAgent,
    userDisplayName,
    userAvatarUrl,
    userBuiltinAvatarId,
    onApprove,
    t,
    scrollContainerRef,
    isLoadingSession,
    loadError,
    messages,
    initialScrollOffset,
    onRetryLoadSession,
  } = props;

  // ── Refs ────────────────────────────────────────────────────────

  // ── Container width (self-measured) ─────────────────────────────
  //
  // VirtualMessageList owns its own width measurement via
  // scrollContainerRef.  The scroll container div exists unconditionally
  // in ChatPanel's JSX, so its ref is available in useLayoutEffect.
  //
  // React guarantees that setState inside useLayoutEffect is flushed
  // synchronously before the browser paints.  This means the
  // ScrollController's init-scroll effect (in ChatPanel) will skip on
  // the first invocation (containerWidth is still 0), then fire on the
  // synchronous re-render (containerWidth > 0), all before the first
  // paint.  No flushSync, no rAF, no force-overscan.
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
  // `blockHeightEstimator.ts`.  ResizeObserver + module-level measurement
  // cache (blockHeightEstimator.ts) feed real DOM heights back into the
  // estimator, so after each block is in view once its height in the
  // virtualizer's measurementsCache is exact.
  const messageBlocksRef = useRef(messageBlocks);
  messageBlocksRef.current = messageBlocks;
  const containerWidthRef = useRef(containerWidth);
  containerWidthRef.current = containerWidth;
  const showCompactingItemRef = useRef(showCompactingItem);
  showCompactingItemRef.current = showCompactingItem;
  const showReplyingItemRef = useRef(showReplyingItem);
  showReplyingItemRef.current = showReplyingItem;

  const estimateSize = React.useCallback(
    (index: number) =>
      estimateBlockHeight(
        index,
        messageBlocksRef.current,
        containerWidthRef.current,
        showCompactingItemRef.current,
        showReplyingItemRef.current,
      ),
    [], // eslint-disable-line react-hooks/exhaustive-deps
  );

  const virtualizer = useVirtualizer({
    count: virtualCount,
    getScrollElement: () => scrollContainerRef.current,
    estimateSize,
    overscan: 5,
    gap: 4,
    initialOffset: initialScrollOffset ?? 0,
    // getItemKey: use content-derived blockId so the virtualizer's internal
    // measurement cache survives prepend/append. Without this, the cache is
    // keyed by index and all measurements shift when items are prepended,
    // making getTotalSize() inaccurate and breaking the scrollHeight delta.
    getItemKey: (index: number) => messageBlocksRef.current[index]?.blockId ?? index,
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

  // ── measureElement + ResizeObserver ──────────────────────────────
  // virtualizer.measureElement fires on mount, but async content (Mermaid
  // 200ms debounce, code highlighting) changes height AFTER mount.  The
  // initial measurement is wrong; we need to re-measure when the element
  // actually settles.
  //
  // Each item gets a ResizeObserver.  On any size change, debounce 300ms
  // then re-call measureElement.  This gradually corrects totalSize toward
  // the true value - the scrollbar "converges" rather than "jumps".
  //
  // Observers are tracked in a WeakMap so they're GC'd when the element is
  // removed from the DOM (virtual list recycling).
  const measureElementRef = useRef<((el: HTMLElement | null) => void) | null>(null);
  measureElementRef.current = virtualizer.measureElement;

  /**
   * Wrapped version of virtualizer.measureElement that catches
   * NotAllowedError from @tanstack/virtual-core's internal ResizeObserver.
   *
   * The virtual-core library calls `ResizeObserver.observe(target, { box: 'border-box' })`
   * inside its ResizeObserver callback.  In WKWebView (Tauri), when the virtual
   * list recycles a DOM element that is still being observed, the observe() call
   * can throw a NotAllowedError.  This is harmless — the element is about to be
   * recycled anyway — but the error propagates through the ResizeObserver, which
   * logs it as "ERR – NotAllowedError" via console.error.  We swallow it here.
   *
   * See: @tanstack/virtual-core/dist/esm/index.js:238 (.observe(target, { box: "border-box" }))
   */
  const safeMeasureElement = useCallback((el: HTMLElement | null) => {
    const fn = measureElementRef.current;
    if (!fn) return;
    try {
      fn(el);
    } catch (e) {
      if (e instanceof DOMException && e.name === "NotAllowedError") {
        // Swallow — element is being recycled, the measurement is moot.
        return;
      }
      // Re-throw unexpected errors so they're not silently hidden.
      throw e;
    }
  }, []);

  const measureRef = useCallback((el: HTMLElement | null) => {
    // measureElement on mount: writes whatever height the item has at the
    // moment of mount into the virtualizer cache.  ResizeObserver below
    // then fires whenever the size changes (e.g. Mermaid SVG finishes
    // rendering) and re-calls measureElement.
    //
    // We also write the observed height into a module-level cache keyed
    // by MessageBlock.blockId (see blockHeightEstimator.ts).  estimateSize
    // consults that cache on the next render, so once an item has been
    // measured its real height is preserved across re-mounts (virtual
    // list recycling) and across re-opens of the same session — fixing
    // the "scroll back to top, scroll bar shrinks, can't reach bottom"
    // cycle that happens when measurement-driven cache writes are lost.
    safeMeasureElement(el);
    if (!el) return;
    const idxAttr = el.dataset.index;
    if (idxAttr !== undefined) {
      const idx = parseInt(idxAttr, 10);
      const block = messageBlocksRef.current[idx];
      if (block && el.offsetHeight > 0) {
        recordMeasuredHeight(block.blockId, el.offsetHeight);
      }
    }
    if (resizeObservers.has(el)) return;
    const ro = new ResizeObserver(() => {
      safeMeasureElement(el);
      // Re-record on every async-render-driven size change.  recordMeasuredHeight
      // no-ops on <2px drift and overwrites on larger drift (e.g. Mermaid SVG
      // finishing its render, growing from ~140px → ~500px).
      const idxAttr2 = el.dataset.index;
      if (idxAttr2 !== undefined) {
        const idx2 = parseInt(idxAttr2, 10);
        const block2 = messageBlocksRef.current[idx2];
        if (block2 && el.offsetHeight > 0) {
          recordMeasuredHeight(block2.blockId, el.offsetHeight);
        }
      }
    });
    ro.observe(el);
    resizeObservers.set(el, ro);
  }, []);

  // ── Imperative handle ─────────────────────────────────────
  // Exposes data-derived queries and scroll actions to the parent
  // (ScrollController). None of the queries rely on scrollTop pixel math
  // or estimateSize-based heuristics; everything comes from
  // virtualizer.getVirtualItems() intersecting the messageBlocks array.
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
      scrollToBottom: () => {
        const count = virtualizer.options.count;
        if (count === 0) return;
        virtualizer.scrollToIndex(count - 1, { align: "end" });
      },
      scrollToTop: () => {
        const count = virtualizer.options.count;
        if (count === 0) return;
        virtualizer.scrollToIndex(0, { align: "start" });
      },
    }),
    [virtualizer, messageBlocks],
  );

  // ── Scroll effects ──
  // All scroll-related effects (init scroll, scrollHeight delta, sticky-bottom,
  // ensure-renderable, jump target) have been moved to useScrollController.ts.
  // VML now only owns: width measurement, virtualizer creation, rendering,
  // and the imperative handle (scrollToBottom/scrollToTop/getFirstVisible/...).

  // ── Render ───────────────────────────────────────────────────────
  return (
    <>
      {/* Loading more indicator at top */}
      {adapter.isLoading && (
        <div className="flex items-center justify-center py-2">
          <span className="inline-block h-4 w-4 animate-spin rounded-full border-2 border-zinc-300 border-t-zinc-600 dark:border-zinc-600 dark:border-t-zinc-300" />
          <span className="ml-1.5 text-[10px] text-zinc-400 dark:text-zinc-500">Loading more...</span>
        </div>
      )}

      {/* Loading session indicator */}
      {isLoadingSession && messages.length === 0 && (
        <div className="absolute inset-0 flex items-center justify-center">
          <div className="text-center">
            <span className="inline-block h-8 w-8 animate-spin rounded-full border-2 border-zinc-300 border-t-zinc-600 dark:border-zinc-600 dark:border-t-zinc-300" />
            <p className="mt-3 text-xs text-zinc-400 dark:text-zinc-500">Loading conversation...</p>
          </div>
        </div>
      )}

      {loadError && !isLoadingSession && (
        <div className="absolute inset-0 flex flex-col items-center justify-center gap-3 px-4">
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
        <div className="absolute inset-0 flex items-center justify-center text-xs text-zinc-400 dark:text-zinc-500">
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
            // Trailing extra-item slot indices.  These match the slot
            // mapping in `estimateBlockHeight` (blockHeightEstimator.ts)
            // and the `virtualCount` math in ChatPanel: replying (when
            // shown) sits IMMEDIATELY after messageBlocks, compacting
            // (when shown) sits LAST.  Keeping these computations local to
            // the renderer means the renderer is self-sufficient and the
            // estimator and virtualCount math don't have to align via
            // ChatPanel.
            //
            // Both branches share identical chrome (`ml-12 py-1.5` + dot
            // + shimmer label) so the upcoming reply lands in the same
            // visual region as the previous placeholder, with no jump.
            const extraCount =
              (showReplyingItem ? 1 : 0) + (showCompactingItem ? 1 : 0);
            const replyingIdx = showReplyingItem ? messageBlocks.length : -1;
            const compactingIdx = showCompactingItem
              ? messageBlocks.length + extraCount - 1
              : -1;

            // --- Replying indicator (extra virtual item, slot 0) ---
            // Same chrome as the thought preview (StreamingSourceBlock
            // variant="thought" inside ExploreBlock) but rendered as a
            // trailing virtual item with ml-12 offset matching the agent
            // message column.  When record_complete freezes the message
            // and `isAssistantReplying` clears, the slot collapses onto
            // the now-real bubble content with no jump — the same layout
            // contract the old "Replying..." dot indicator provided, but
            // now showing live streamed text instead of a static label.
            if (virtualRow.index === replyingIdx) {
              return (
                <div
                  key={virtualRow.key}
                  ref={measureRef}
                  data-index={virtualRow.index}
                  style={{
                    position: "absolute",
                    top: 0,
                    left: 0,
                    width: "100%",
                    transform: `translateY(${virtualRow.start}px)`,
                  }}
                >
                  <div className="ml-12" aria-label={t("chatPanel.replying")}>
                    <StreamingSourceBlock
                      content={assistantStreamingContent}
                      isStreaming={true}
                      startTime={assistantStreamingStartTime ?? undefined}
                      variant="assistant"
                      showTruncationNotice={false}
                    />
                  </div>
                </div>
              );
            }

            // --- Compacting indicator (extra virtual item, last slot) ---
            if (virtualRow.index === compactingIdx) {
              return (
                <div
                  key={virtualRow.key}
                  ref={measureRef}
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
                ref={measureRef}
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
                      prevType === "system"
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
                      && t !== "compaction");
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
                  // ADR-035 convergent model: no placeholder messages with
                  // isStreaming=true exist in messages[] anymore.  The
                  // streaming state is driven solely by `sending` (which
                  // reflects isAssistantReplying / session active status).
                  const isStreamingGroup = sending && isLastGroup;
                  // Live thought attaches ONLY to the active (last) explore
                  // group.  Historical explore groups must never render the
                  // streaming ThinkBlock — otherwise every expanded past
                  // block would show a phantom "正在思考..." entry because
                  // their items[] lack a frozen thought message, satisfying
                  // ExploreBlock's `liveThoughtNotYetLoaded` predicate.
                  // Converging all three live-thought props through this
                  // single flag ensures a future internal change in
                  // ExploreBlock cannot regress this scope again.
                  const liveThoughtAttaches = isThinking && isLastGroup;
                  return (
                    <div className="ml-12">
                      <ExploreBlock
                        items={item.items}
                        isStreaming={isStreamingGroup}
                        pendingApproval={pendingApproval}
                        currentSessionId={currentSessionId}
                        onApprove={(action, approval) => onApprove(action, approval)}
                        onCancelTool={onCancelTool}
                        toolProgress={toolProgress}
                        hasFollowUpReply={hasFollowUpReply}
                        isThinking={liveThoughtAttaches}
                        thinkingContent={liveThoughtAttaches ? thinkingContent : ""}
                        thinkingStartTime={liveThoughtAttaches ? thinkingStartTime : null}
                      />
                    </div>
                  );
                })()}

                {/* Regular message */}
                {item.type !== "explore_group" && item.type !== "user_with_attachments" && (() => {
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

                {/* User message with attachments */}
                {item.type === "user_with_attachments" && (() => {
                  const userMsg = item.items[0];
                  const attachments = item.items.slice(1);
                  return (
                    <UserWithAttachmentsBubble
                      userMessage={userMsg}
                      attachments={attachments}
                      currentSessionId={currentSessionId ?? ""}
                      liveUserName={userDisplayName}
                      liveUserAvatarUrl={userAvatarUrl}
                      liveUserBuiltinAvatarId={userBuiltinAvatarId}
                      agentId={selectedAgentId as string | undefined}
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
