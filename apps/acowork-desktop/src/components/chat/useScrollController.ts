/**
 * useScrollController - Event-driven pagination + scroll-position
 * controller for the chat list.
 *
 * ADR-050 C4 — replaces the v1 782-line state machine with a ~150-line
 * event-driven pipeline.  The controller is the ONLY layer that reads
 * DOM (scrollTop / scrollHeight) and writes scroll-side effects.  It
 * does NOT carry any per-render scroll state — every value is derived
 * from either DOM (read on demand) or the adapter (data source).
 *
 * Adapter compatibility
 * ---------------------
 * The controller accepts both v1 `ChatListAdapter` (still consumed by
 * ChatPanel in C4) and v2 `ChatListAdapterV2` — they share the same
 * read shape (`isAtTail / hasPendingFlush / hasOlder / hasNewer /
 * loadPrevPage / loadNextPage / scrollToBottom / scrollToTop /
 * subscribe`).  C5 will drop v1 entirely; once ChatPanel binds v2,
 * the controller's `adapter` type narrows to `ChatListAdapterV2`.
 *
 * Responsibilities
 * ----------------
 * 1. **Pagination tick** (150ms `setInterval`):
 *      - If user is near the TOP of the scroll container, call
 *        `adapter.loadPrevPage()`.
 *      - If user is near the BOTTOM, call `adapter.loadNextPage()`.
 *      - Otherwise, idle.
 *
 * 2. **Scroll arrow visibility** (data-driven, no DOM read):
 *      - `showScrollToBottom` = `!adapter.isAtTail() || adapter.hasPendingFlush()`.
 *      - `showScrollToTop`    = `adapter.hasOlder`.
 *
 * 3. **Jump buttons** (delegate to the adapter):
 *      - `jumpToBottom` = `adapter.scrollToBottom()`
 *      - `jumpToTop`    = `adapter.scrollToTop()`
 *
 * 4. **Event subscription** (C4 new):
 *      The controller subscribes to `adapter.subscribe(...)` and on
 *      `liveUpdate` events would check whether the streaming block is
 *      in the viewport (via `vmlRef.getLastVisibleBlockIndex`).  When
 *      in viewport, the VML is asked to refresh its streaming block
 *      (`vmlRef.refreshStreamingBlock` — added in C5).  When out of
 *      viewport, the controller skips the refresh entirely to avoid
 *      off-screen DOM churn.  For C4 the subscription is a no-op
 *      (vmlRef doesn't yet expose `refreshStreamingBlock`); C5 will
 *      wire the full chain.
 *
 * Why no state
 * ------------
 * Pre-ADR-050, the controller carried a 5-state machine
 * (pinned-bottom / idle / loading-older / loading-newer / jumping)
 * with `prevScrollHeightRef` deltas and `wasAtBottomRef` ground-truth
 * tracking.  This state was duplicated by `chatStore.isPinnedToBottom`
 * and competed with `useChatListAdapter.isAtTail()` for the
 * "is the user at the bottom?" question.  ADR-050 collapses all three
 * into a single adapter-derived signal (`isAtTail` /
 * `hasPendingFlush`); the controller is reduced to a DOM-reading
 * + adapter-calling shim.
 *
 * "Auto-follow when streaming" behavior:
 * Pre-ADR-050, the controller's sticky-bottom logic forced the scroll
 * position to the bottom whenever a new message arrived while the
 * user was pinned.  ADR-050 C4 DROPS this behavior (per user feedback:
 * "用户滚到哪里显示哪里").  When the user is at the bottom, the
 * BROWSER naturally keeps them there as `scrollHeight` grows.  When
 * the user scrolls away, the position is preserved — they see new
 * content accumulate BELOW the current viewport.  Clicking the
 * "jump to latest" button re-anchors at any time.
 */
import { useCallback, useEffect, useRef } from "react";
import type { ChatListAdapter as ChatListAdapterV1 } from "./useChatListAdapter";
import type { ChatListAdapterV2 } from "./chatListAdapter";
import type { VirtualMessageListHandle } from "./VirtualMessageList";

/**
 * Minimal adapter shape required by the C4 controller.  Both v1
 * (`useChatListAdapter.ts`) and v2 (`chatListAdapter.ts`) satisfy
 * this; C5 will drop v1 and the controller will accept
 * `ChatListAdapterV2` directly.
 */
type ControllerAdapter = ChatListAdapterV1 | ChatListAdapterV2;

// ── Constants ──────────────────────────────────────────────────────────────

/**
 * Distance from the top edge of the scroll container (in px) below
 * which we trigger `loadPrevPage`.  Small enough that prepending is
 * seamless (the user doesn't see the threshold line) and large enough
 * that a single scroll event doesn't oscillate between triggering and
 * idle.
 */
const EDGE_THRESHOLD_PX = 50;

/**
 * Polling cadence for the pagination tick.  The controller reads DOM
 * (scrollTop / scrollHeight) on each tick, not via scroll listeners,
 * so the rate caps the "load older / newer" trigger rate regardless of
 * how fast the user scrolls.  150ms ≈ 6.6 Hz — fast enough to keep
 * up with scroll-driven loads, slow enough that a brief stall during
 * loadPrevPage doesn't immediately re-trigger.
 *
 * Pre-ADR-050, the controller relied on scroll listeners + a state
 * machine to throttle loads.  C4 simplifies: tick at 6.6Hz, call the
 * adapter, done.  The adapter itself deduplicates concurrent loads
 * via `isLoading`.
 */
const TIMER_INTERVAL_MS = 150;

// ── Public types ───────────────────────────────────────────────────────────

export interface ScrollController {
  /** Show the floating "jump to latest" arrow. */
  showScrollToBottom: boolean;
  /** Show the floating "jump to oldest" arrow. */
  showScrollToTop: boolean;
  /** Subscribe to the scroll container's onScroll event. */
  handleScroll: () => void;
  /** User clicked "jump to latest". */
  jumpToBottom: () => void;
  /** User clicked "jump to oldest". */
  jumpToTop: () => void;
  /** Whether the user is currently pinned to the bottom. */
  isPinnedToBottom: () => boolean;
}

export interface ScrollControllerConfig {
  /** Ref to the scroll container (owned by ChatPanel). */
  containerRef: React.RefObject<HTMLDivElement | null>;
  /** The chat list adapter (v1 OR v2). */
  adapter: ControllerAdapter;
  /** Imperative handle to VirtualMessageList (scrollToBottom / scrollToTop). */
  vmlRef: React.RefObject<VirtualMessageListHandle | null>;
  /** Session key — used to reset the tick on session change. */
  sessionKey: string | null;

  // ── C4 deprecation shims ──
  // The fields below are accepted but ignored by the C4 controller.
  // They are kept in the config interface so ChatPanel can keep
  // passing the same shape; C5 will remove the callsite once the
  // v2 adapter fully replaces v1.
  /** @deprecated Removed in C4 — C5 caller cleanup. */
  sending?: boolean;
  /** @deprecated Removed in C4 — replaced by adapter.totalBlocks. */
  virtualCount?: number;
  /** @deprecated Removed in C4 — adapter now owns block bookkeeping. */
  messageBlocks?: unknown[];
  /** @deprecated Removed in C4 — nav-back scroll restore deferred to a
   *  follow-up; C5 will wire `sessionStorage`-backed initial offset. */
  initialScrollOffset?: number;
  /** @deprecated Removed in C4 — adapter is the single source of truth
   *  for scroll position; agentId/sessionId no longer need to be passed
   *  through the controller. */
  agentId?: string | null;
  /** @deprecated Removed in C4 — see agentId note above. */
  sessionId?: string | null;
}

// ── Hook ───────────────────────────────────────────────────────────────────

export function useScrollController(config: ScrollControllerConfig): ScrollController {
  const { containerRef, adapter, vmlRef, sessionKey } = config;

  // Mirrors of the latest adapter / vmlRef / container refs so the
  // interval callback (registered with stable deps) reads current
  // values without re-subscribing on every render.  This is the ONLY
  // "state" the controller carries; it is not used to drive UI
  // re-renders.
  const adapterRef = useRef(adapter);
  adapterRef.current = adapter;
  const vmlRefRef = useRef(vmlRef);
  vmlRefRef.current = vmlRef;
  const containerRefRef = useRef(containerRef);
  containerRefRef.current = containerRef;

  // `showScrollToBottom` is data-driven.  Read directly from the
  // adapter on every render — no useState, no useMemo.  React's
  // re-render is already triggered by `adapter` updating through
  // `useChatListAdapter`, so deriving flags inline is free.
  const showScrollToBottom = !adapter.isAtTail() || adapter.hasPendingFlush();
  const showScrollToTop = adapter.hasOlder;

  // ── Pagination tick ──
  // 150ms setInterval.  On each tick, read scrollTop from the DOM and
  // decide whether to loadPrevPage / loadNextPage.  The interval is
  // stable across renders; only restart on sessionKey change.
  useEffect(() => {
    const tick = () => {
      const container = containerRefRef.current.current;
      const a = adapterRef.current;
      if (!container || !a) return;
      if (a.isLoading) return;
      const scrollTop = container.scrollTop;
      // near-top → load older
      if (scrollTop <= EDGE_THRESHOLD_PX && a.hasOlder) {
        void a.loadPrevPage();
        return;
      }
      // near-bottom → load newer
      const distFromBottom =
        container.scrollHeight - (scrollTop + container.clientHeight);
      if (distFromBottom <= EDGE_THRESHOLD_PX && a.hasNewer) {
        void a.loadNextPage();
      }
    };
    const id = window.setInterval(tick, TIMER_INTERVAL_MS);
    return () => {
      window.clearInterval(id);
    };
  }, [sessionKey]);

  // ── Event subscription ──
  // The controller subscribes to adapter events.  On `liveUpdate`,
  // check whether the streaming block is in viewport and ask VML to
  // refresh it; otherwise skip to avoid off-screen DOM churn.
  //
  // C4 keeps this as a no-op stub — VML's `isStreamingBlockInViewport`
  // and `refreshStreamingBlock` are added in C5.  Until then the
  // subscription is registered but does nothing.
  useEffect(() => {
    const unsub = adapter.subscribe((event) => {
      if (event.type !== "liveUpdate") return;
      const vml = vmlRefRef.current.current;
      if (!vml) return;
      // C5 will add: vml.isStreamingBlockInViewport() + vml.refreshStreamingBlock()
      // For C4 we no-op so the subscription contract is testable.
      void vml;
    });
    return unsub;
  }, [adapter, sessionKey]);

  // ── handleScroll ──
  // Called from the scroll container's onScroll event.  The handler
  // exists to satisfy the legacy interface (ChatPanel still binds
  // onScroll) and to give the React event loop a chance to re-render
  // the showScrollToTop flag (which depends on hasOlder — already
  // driven by adapter re-renders).  C4 does not need DOM-side work
  // here; the 150ms tick already covers near-edge detection.
  const handleScroll = useCallback(() => {
    // Intentionally empty: the 150ms tick reads DOM, the adapter
    // re-renders drive showScrollToBottom.  Keeping the callback as
    // a no-op preserves the public interface for ChatPanel's
    // onScroll binding without re-introducing scroll listeners.
  }, []);

  // ── Jump primitives ──
  // Both jumps delegate to the adapter, which owns the
  // "load-then-scroll" walk.
  const jumpToBottom = useCallback(() => {
    void adapter.scrollToBottom();
  }, [adapter]);

  const jumpToTop = useCallback(() => {
    void adapter.scrollToTop();
  }, [adapter]);

  // ── isPinnedToBottom ──
  // ADR-050 §7 C4: the pre-existing `isPinnedToBottom` getter is
  // preserved so ChatPanel's existing `isPinnedToBottomRef`-style
  // consumers keep working.  The semantics are the SAME as
  // `showScrollToBottom` (data-driven, no DOM read).  C5 will remove
  // this once ChatPanel's consumer is fully migrated.
  const isPinnedToBottom = useCallback((): boolean => {
    return adapter.isAtTail() && !adapter.hasPendingFlush();
  }, [adapter]);

  return {
    showScrollToBottom,
    showScrollToTop,
    handleScroll,
    jumpToBottom,
    jumpToTop,
    isPinnedToBottom,
  };
}