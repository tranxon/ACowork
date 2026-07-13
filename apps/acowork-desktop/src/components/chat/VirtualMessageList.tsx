import React, { useLayoutEffect, useRef } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import type { ChatMessage, ToolApprovalNeededEvent } from "../../lib/types";
import { AgentAvatar } from "../common/AgentAvatar";
import { ExploreBlock } from "./ExploreBlock";
import { MessageBubble } from "./MessageBubble";
import type { SessionScope } from "./useSessionScope";

// ── Types ────────────────────────────────────────────────────────────

interface VirtualMessageListProps {
  /** Grouped display messages (from useMemo in ChatPanel). */
  displayMessages: Array<
    | ChatMessage
    | { type: "explore_group"; items: ChatMessage[] }
  >;
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
  /** Per-session scope ref (for isLoadingMore, prevScrollHeight, prevStickyCount, etc.). */
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

// ── Component ────────────────────────────────────────────────────────

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
export function VirtualMessageList(props: VirtualMessageListProps) {
  const {
    displayMessages,
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
    isLoadingSession,
    loadError,
    messages,
    initialScrollOffset,
    onRetryLoadSession,
  } = props;

  // ── Virtualizer ──────────────────────────────────────────────────
  // Created fresh on every mount (thanks to parent's key={currentScrollKey}).
  // initialOffset: 0 ensures getScrollOffset() returns 0 on the first render,
  // before any scroll event has fired.
  const virtualizer = useVirtualizer({
    count: virtualCount,
    getScrollElement: () => scrollContainerRef.current,
    estimateSize: () => 80,
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

  // ── Scroll-to-bottom or restore position on mount ───────────────
  // On fresh mount (session switch), scroll to bottom synchronously.
  // On nav-back with saved scroll offset, restore the position.
  // The parent's key={currentScrollKey} ensures this component remounts
  // on every session/agent switch, so this effect always runs fresh.
  const didInitialScrollRef = useRef(false);
  useLayoutEffect(() => {
    const container = scrollContainerRef.current;
    // DIAGNOSTIC
    console.log("[VML:mount]", { virtualCount, initialScrollOffset, sending, hasContainer: !!container, containerScrollHeight: container?.scrollHeight });

    if (virtualCount === 0) {
      // ADR-026: Empty session on mount — still mark initialized so the
      // sticky-bottom effect (below) will fire when the first message arrives.
      // Without this, didInitialScrollRef stays false forever, the sticky
      // guard (!didInitialScrollRef.current) always returns early, and new
      // streaming content never auto-scrolls into view.
      didInitialScrollRef.current = true;
      pinnedToBottomRef.current = true;
      scope.current.prevStickyCount = 0;
      return;
    }

    if (!container) return;

    if (initialScrollOffset !== undefined && initialScrollOffset >= 0) {
      // Nav-back: restore saved scroll position. The Virtualizer was
      // created with initialOffset=initialScrollOffset, so getVirtualItems()
      // renders items at the correct offset from the first frame. Sync the
      // actual DOM scrollTop to match.
      container.scrollTop = initialScrollOffset;
      pinnedToBottomRef.current = false;
    } else {
      // Fresh session: scroll to bottom synchronously before paint.
      // Reset scrollTop to 0 first so a scroll event always fires.
      container.scrollTop = 0;
      virtualizer.scrollToIndex(virtualCount - 1, { align: "end" });
      pinnedToBottomRef.current = true;
    }

    didInitialScrollRef.current = true;
    // DIAGNOSTIC: log state after one frame
    requestAnimationFrame(() => {
      const c = scrollContainerRef.current;
      console.log("[VML:mount+1f]", {
        scrollTop: c?.scrollTop, scrollHeight: c?.scrollHeight, clientHeight: c?.clientHeight,
        distFromBottom: c ? c.scrollHeight - c.scrollTop - c.clientHeight : null,
        pinnedToBottomRef: pinnedToBottomRef.current, virtualCount,
      });
    });
  }, []); // eslint-disable-line react-hooks/exhaustive-deps
  // Only run on mount — the parent's key={currentScrollKey} ensures this
  // component remounts on every session switch.

  // ── Load-more: restore scroll position after messages prepended ────
  // When the user scrolls to the top and "load more" fires, ChatPanel sets
  // scope.current.isLoadingMore=true and captures prevScrollHeight.  After
  // new messages are prepended (isLoadingMore→false), we restore the
  // previous scroll offset so the user stays at the same visual position.
  const prevIsLoadingMoreRef = useRef(false);
  useLayoutEffect(() => {
    const wasLoading = prevIsLoadingMoreRef.current;
    prevIsLoadingMoreRef.current = isLoadingMore;
    if (!wasLoading || isLoadingMore) return;
    if (scope.current.prevScrollHeight <= 0) return;
    const offset = scope.current.prevScrollHeight;
    scope.current.prevScrollHeight = 0;
    virtualizer.scrollToOffset(offset, { align: "start" });
  }, [isLoadingMore, virtualizer, scope]);

  // ── Sticky-bottom: keep pinned when new items arrive ──────────────
  const prevTotalSizeRef = useRef(0);
  const totalSize = virtualizer.getTotalSize();
  useLayoutEffect(() => {
    if (!didInitialScrollRef.current) return;
    const countChanged = virtualCount !== scope.current.prevStickyCount;
    const sizeChanged = totalSize !== prevTotalSizeRef.current;
    scope.current.prevStickyCount = virtualCount;
    prevTotalSizeRef.current = totalSize;

    // Scroll to bottom when content changes (new items OR existing item
    // growth) AND the user is currently pinned to the bottom.  During
    // streaming a single message grows continuously — checking only
    // countChanged would skip those growth events and the user would
    // gradually drift upward.
    if (virtualCount > 0 && (countChanged || sizeChanged) && pinnedToBottomRef.current) {
      // Skip if the container is already within 1px of the bottom — avoids
      // racing with ChatPanel's smooth scrollToBottom that already set the
      // position via element.scrollTo({ behavior: "smooth" }).
      const container = scrollContainerRef.current;
      if (container) {
        const distFromBottom = container.scrollHeight - container.scrollTop - container.clientHeight;
        if (distFromBottom <= 1) return;
      }
      virtualizer.scrollToIndex(virtualCount - 1, { align: "end" });
    }
  }, [totalSize, virtualCount, virtualizer, scope, pinnedToBottomRef]);

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
      {displayMessages.length > 0 && (
        <div
          style={{
            height: virtualizer.getTotalSize(),
            width: "100%",
            position: "relative",
          }}
        >
          {virtualizer.getVirtualItems().map((virtualRow) => {
            // Compacting indicator is the only extra virtual item.
            const compactingIdx = displayMessages.length;

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
            const item = displayMessages[virtualRow.index];
            const displayItem = item as any;

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
                    const prev = displayMessages[i];
                    const prevType = "type" in prev
                      ? (prev as ChatMessage).type
                      : "explore_group";
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
                  const isAgent = displayItem.type === "explore_group"
                    || (displayItem.type !== "explore_group"
                      && (item as ChatMessage).type !== "user"
                      && (item as ChatMessage).type !== "system"
                      && (item as ChatMessage).type !== "compaction"
                      && (item as ChatMessage).type !== "document_upload");
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
                {displayItem.type === "explore_group" && (() => {
                  const nextItem = displayMessages[virtualRow.index + 1];
                  const hasFollowUpReply = nextItem !== undefined && (nextItem as any).type !== "explore_group";
                  const isLastGroup = virtualRow.index === displayMessages.length - 1;
                  const isStreamingGroup = sending && isLastGroup
                    && displayItem.items.some((it: ChatMessage) => it.isStreaming === true);
                  return (
                    <div className="ml-12">
                      <ExploreBlock
                        items={displayItem.items}
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
                {displayItem.type !== "explore_group" && (() => {
                  const msg = item as ChatMessage;
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
}
