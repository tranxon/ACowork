/**
 * useScrollController - Centralized scroll state machine for the chat message list.
 *
 * Owns ALL scroll-related state and decisions:
 *  - Scroll position tracking (pinned-to-bottom vs free-scroll)
 *  - Arrow button visibility (showScrollToBottom / showScrollToTop)
 *  - Pagination triggers (loadBefore / loadAfter)
 *  - scrollTop adjustment after prepend (scrollHeight delta)
 *  - Auto-scroll on new messages (sticky-bottom)
 *  - ensureRenderable (fill viewport)
 *  - Jump-to-top / jump-to-bottom (via adapter.jumpTarget)
 *
 * The state machine prevents concurrent operations and race conditions
 * that were previously caused by multiple independent code paths (timer,
 * onLayout, sticky-bottom effect, scrollHeight delta effect, handleScroll)
 * reading the DOM and making uncoordinated decisions.
 *
 * States:
 *  - pinned-bottom: User is at the bottom; new messages auto-scroll.
 *  - idle: User is freely browsing; no operation in progress.
 *  - loading-older: loadBefore in progress; scrollTop will be adjusted on completion.
 *  - loading-newer: loadAfter in progress; no scrollTop adjustment needed.
 *  - jumping: User clicked an arrow button; waiting for data + scroll.
 *
 * Architecture:
 *  ChatPanel creates the controller via useScrollController().
 *  VML no longer has its own scroll effects (sticky-bottom, scrollHeight delta,
 *  ensureRenderable, jump target, init scroll). All of these are owned by
 *  the controller and run as useLayoutEffects in ChatPanel's render cycle
 *  (which runs AFTER VML's effects, so the DOM is up-to-date).
 */

import { useRef, useState, useCallback, useEffect, useLayoutEffect } from "react";
import type { ChatListAdapter } from "./useChatListAdapter";
import type { VirtualMessageListHandle } from "./VirtualMessageList";
import type { MessageBlock } from "./messageFolder";
import { useChatStore } from "../../stores/chatStore";
import { log } from "../../lib/logger";

// ── Constants ────────────────────────────────────────────────────────────

/**
 * Distance from bottom (in px) within which we consider the user "pinned
 * to the bottom".  When the user scrolls up beyond this threshold:
 *  - State machine transitions from pinned-bottom to idle.
 *  - isPinnedToBottom is set to false in the store (via setPinnedToBottom).
 *  - Down-arrow button appears (same threshold, unified).
 *  - Auto-scroll on new messages is disabled.
 *  - thinkingContent flush and scheduleRefresh are blocked.
 */
const PIN_THRESHOLD_PX = 120;

/** Distance from top/bottom edge (in px) that triggers pagination. */
const EDGE_THRESHOLD_PX = 50;

/** Interval for the pagination timer. */
const TIMER_INTERVAL_MS = 150;

/** Max number of ensureRenderable pages to load (prevents infinite loop). */
const MAX_ENSURE_RENDERABLE_PAGES = 10;

// ── Types ────────────────────────────────────────────────────────────────

type ScrollState = "pinned-bottom" | "idle" | "loading-older" | "loading-newer" | "jumping";

export interface ScrollController {
  /** Whether to show the scroll-to-bottom arrow button. */
  showScrollToBottom: boolean;
  /** Whether to show the scroll-to-top arrow button. */
  showScrollToTop: boolean;
  /** Called from the scroll container's onScroll event. */
  handleScroll: () => void;
  /** Called when the user clicks the scroll-to-bottom button. */
  jumpToBottom: () => void;
  /** Called when the user clicks the scroll-to-top button. */
  jumpToTop: () => void;
  /** Whether the user is currently pinned to the bottom. */
  isPinnedToBottom: () => boolean;
}

interface ScrollControllerConfig {
  /** Ref to the scroll container (owned by ChatPanel). */
  containerRef: React.RefObject<HTMLDivElement | null>;
  /** The chat list adapter (pagination state + actions). */
  adapter: ChatListAdapter;
  /** Imperative handle to VirtualMessageList (scrollToBottom, scrollToTop). */
  vmlRef: React.RefObject<VirtualMessageListHandle | null>;
  /** Whether the session is currently streaming (affects init scroll). */
  sending: boolean;
  /** Total virtual item count (messageBlocks + trailing extras). */
  virtualCount: number;
  /** Folded message blocks (used for prepend detection). */
  messageBlocks: MessageBlock[];
  /** If provided, restore this scroll offset on init (nav-back). */
  initialScrollOffset?: number;
  /** Session key (for reset on session change). */
  sessionKey: string | null;
  /** Agent ID – used to sync isPinnedToBottom to the store. */
  agentId: string | null;
  /** Session ID – used to sync isPinnedToBottom to the store. */
  sessionId: string | null;
}

// ── Hook ────────────────────────────────────────────────────────────────

export function useScrollController(config: ScrollControllerConfig): ScrollController {
  const {
    containerRef,
    adapter,
    vmlRef,
    sending,
    virtualCount,
    messageBlocks,
    initialScrollOffset,
    sessionKey,
    agentId,
    sessionId,
  } = config;

  // ── Refs for stable callbacks ──
  // agentId/sessionId are read inside transitionTo (which has empty deps
  // for stable identity).  Refs ensure we always read the latest values.
  const agentIdRef = useRef(agentId);
  agentIdRef.current = agentId;
  const sessionIdRef = useRef(sessionId);
  sessionIdRef.current = sessionId;

  // ── State machine (ref, no re-render on transition) ──
  const stateRef = useRef<ScrollState>("pinned-bottom");

  // ── UI state (triggers re-render) ──
  const [showScrollToBottom, setShowScrollToBottom] = useState(false);
  const [showScrollToTop, setShowScrollToTop] = useState(false);

  // ── Tracking refs ──
  const prevScrollHeightRef = useRef(0);

  // Ground-truth scroll position from the last handleScroll call.
  // Updated on EVERY scroll event, INCLUDING during loading/jumping
  // states (before the early return).  The state machine (stateRef) is
  // NOT updated during those states, so it can be temporarily out of
  // sync with the DOM.  §3 (sticky-bottom) uses wasAtBottomRef as a
  // defense-in-depth check: if stateRef says "pinned-bottom" but
  // wasAtBottomRef says the user is NOT at the bottom, the state is
  // stale - correct it and skip the scroll.
  //
  // Unlike distFromBottom (read inside §3's useLayoutEffect, which
  // already includes the new content's height), wasAtBottomRef reflects
  // the user's position BEFORE the new content arrived, so it correctly
  // distinguishes "user at top" from "user at bottom, large message".
  const wasAtBottomRef = useRef(true);
  // Track the first MESSAGE ID (not block ID) for prepend detection.
  // blockId is `block-${items[0].id}` (see messageFolder.ts), but when
  // loadBefore prepends tool_call/thought messages that merge with the
  // existing first explore_group block, the old blockId disappears from
  // the array.  Message IDs are stable across block merges, so tracking
  // the first message ID is more robust.
  const prevFirstMsgIdRef = useRef<string | null>(null);
  const prevVirtualCountRef = useRef(0);
  const didInitScrollRef = useRef(false);
  const ensureRenderableCountRef = useRef(0);
  const prevSessionKeyRef = useRef<string | null>(null);
  // Save the state BEFORE transitioning to a loading state, so the
  // scrollHeight delta and loading cleanup can restore it correctly.
  // Without this, loadBefore triggered from "pinned-bottom" (e.g. by
  // ensureRenderable) would incorrectly transition to "idle", breaking
  // auto-follow.
  const preLoadStateRef = useRef<ScrollState>("pinned-bottom");

  // Mirror of adapter - updated on every render so the pagination timer
  // (a setInterval with stable deps) always reads the latest values.
  // Without this, the timer's useEffect would depend on `adapter`, which
  // changes on every data update (streaming poll, pagination, folding),
  // causing clearInterval/setInterval churn and unnecessary closure
  // allocation.
  const adapterRef = useRef(adapter);
  adapterRef.current = adapter;

  // ── Helpers ──

  const transitionTo = useCallback((newState: ScrollState) => {
    // INVARIANT: when content doesn't overflow the viewport, the user is
    // always "at the bottom" — there is nowhere else to scroll to.  Any
    // attempt to transition to "idle" in this state is a bug: the user
    // cannot be "browsing history" when history fits on one screen.
    //
    // This guard is the single choke point that makes the state machine
    // robust regardless of which effect triggered the transition:
    //   - §1 init scroll restoring a stale scrollOffset
    //   - §3 sticky-bottom with a stale wasAtBottomRef
    //   - §6 loading cleanup on a short conversation
    //   - handleScroll firing during a layout reflow
    // All of these may try to set "idle" when there's no scrollbar; the
    // guard redirects them to "pinned-bottom", preserving streaming
    // auto-follow and scheduleRefresh.
    if (newState === "idle") {
      const container = containerRef.current;
      if (container && container.scrollHeight <= container.clientHeight) {
        newState = "pinned-bottom";
      }
    }

    if (stateRef.current === newState) return;
    log.debug("[ScrollController] transition", {
      from: stateRef.current,
      to: newState,
    });
    const oldState = stateRef.current;
    stateRef.current = newState;
    // Sync isPinnedToBottom to store ONLY on direct transitions between
    // "pinned-bottom" and "idle".  Intermediate states (loading-older,
    // loading-newer, jumping) must NOT trigger store updates:
    //  - They are transient (the state machine will return to pinned-bottom
    //    or idle shortly after).
    //  - A false->true flip on the return trip would trigger a spurious
    //    scheduleRefresh catch-up.
    //
    // Allowed pin-state-changing transitions:
    //   pinned-bottom -> idle         (user scrolled up)
    //   idle -> pinned-bottom         (user scrolled back to bottom)
    // All other transitions (e.g. pinned-bottom -> loading-newer) leave
    // isPinnedToBottom unchanged in the store.
    const isPinned = newState === "pinned-bottom";
    const wasPinned = oldState === "pinned-bottom";
    if (isPinned !== wasPinned) {
      // Only sync when both old and new states are "settled" (not loading/jumping).
      const isSettled = (s: ScrollState) => s === "pinned-bottom" || s === "idle";
      if (isSettled(oldState) && isSettled(newState)) {
        const aid = agentIdRef.current;
        const sid = sessionIdRef.current;
        if (aid && sid) {
          useChatStore.getState().setPinnedToBottom(aid, sid, isPinned);
        }
      }
    }
  }, []);

  // ── Session reset ──
  // When the session changes, reset all state to defaults.  Runs as
  // useLayoutEffect (NOT useEffect) because the order matters:
  //
  //   useLayoutEffect cleanup + setup run inside the commit phase,
  //   BEFORE paint.  Within the same component, hooks execute in
  //   source order — so this session-reset effect runs BEFORE the
  //   init-scroll useLayoutEffect below.  That ordering is critical:
  //   it guarantees didInitScrollRef.current = false is visible to the
  //   init-scroll effect on the SAME commit, so the init-scroll can
  //   actually run scrollToBottom() for the new session.
  //
  // If we used useEffect (post-paint, asynchronous):
  //   1. Commit phase: init-scroll runs first, sees didInitScrollRef
  //      still == true from the previous session, early-returns →
  //      scroll position is NEVER set for the new session.
  //   2. Paint happens with scrollTop at whatever the browser left it
  //      (typically 0).
  //   3. THEN the session-reset useEffect runs and resets the ref —
  //      but init-scroll's deps ([virtualCount, initialScrollOffset,
  //      sending, ...]) haven't changed, so it won't re-run.  The user
  //      lands at the top of every freshly-mounted session.
  //
  // This was the root cause of both:
  //   - "streaming session → switch → switch back lands at top"
  //     (scrollToBottom never fired for the returning session).
  //   - "idle session → switch → switch back lands at top"
  //     (init-scroll never ran to apply the saved scrollOffset either).
  useLayoutEffect(() => {
    if (prevSessionKeyRef.current === sessionKey) return;
    prevSessionKeyRef.current = sessionKey;
    log.debug("[ScrollController] session reset", { sessionKey });
    stateRef.current = "pinned-bottom";
    prevScrollHeightRef.current = 0;
    prevFirstMsgIdRef.current = null;
    prevVirtualCountRef.current = 0;
    didInitScrollRef.current = false;
    ensureRenderableCountRef.current = 0;
    preLoadStateRef.current = "pinned-bottom";
    wasAtBottomRef.current = true;
    setShowScrollToBottom(false);
    setShowScrollToTop(false);
    // isPinnedToBottom is NOT explicitly synced here.  The new session's
    // DEFAULT_SESSION_STATE.isPinnedToBottom is already true, and the
    // state machine starts as "pinned-bottom".  The first handleScroll
    // or transitionTo will sync naturally if needed.
  }, [sessionKey]);

  // ── 1. Init scroll ──
  // Scrolls to the bottom (or restores initialScrollOffset) on the first
  // data arrival.  Runs as a useLayoutEffect so the scroll position is
  // set before the browser paints (no flash of wrong position).
  //
  // Waits for container.clientWidth > 0 so estimateSize produces accurate
  // per-block heights - scrollToIndex(end) lands on the real bottom on
  // the first call.
  useLayoutEffect(() => {
    if (didInitScrollRef.current) return;
    if (virtualCount === 0) return; // wait for data

    const container = containerRef.current;
    if (!container || container.clientWidth <= 0) return; // wait for layout

    didInitScrollRef.current = true;

    if (!sending && initialScrollOffset !== undefined && initialScrollOffset >= 0) {
      container.scrollTop = initialScrollOffset;
      transitionTo("idle");
      wasAtBottomRef.current = false;
      log.debug("[ScrollController] init scroll: restored offset", { initialScrollOffset });
    } else {
      vmlRef.current?.scrollToBottom();
      transitionTo("pinned-bottom");
      wasAtBottomRef.current = true;
      log.debug("[ScrollController] init scroll: bottom");
    }
  }, [virtualCount, initialScrollOffset, sending, containerRef, vmlRef, transitionTo]);

  // ── 2. scrollHeight delta (prepend adjustment) ──
  //
  // Classic infinite-scroll technique. When loadBefore prepends data:
  //   1. Before prepend: scrollHeight = H_old (recorded in prevScrollHeightRef)
  //   2. After prepend:  scrollHeight = H_new (current container.scrollHeight)
  //   3. delta = H_new - H_old = height of prepended content
  //   4. scrollTop += delta -> user's viewport stays on the same content
  //
  // Detection: compare the first MESSAGE ID (not block ID). When
  // loadBefore prepends tool_call/thought messages that merge with the
  // existing first explore_group, the old blockId disappears from the
  // array (the block is re-formed with a new first item).  Message IDs
  // are stable across block merges, so comparing the first message ID
  // correctly detects prepends in all cases.
  //
  // State transition: restore preLoadStateRef (saved before the load
  // started) instead of always transitioning to "idle".  This preserves
  // the user's intent: if they were in "pinned-bottom" (e.g.
  // ensureRenderable triggered loadBefore while at the bottom), the
  // state should return to "pinned-bottom" so streaming continues to
  // auto-follow.
  useLayoutEffect(() => {
    const container = containerRef.current;
    if (!container) return;

    const currentFirstMsgId = messageBlocks[0]?.items[0]?.id ?? null;

    // Skip prepend detection on session change.  The session reset
    // (useEffect) hasn't run yet, so prevFirstMsgIdRef still holds the
    // old session's value.  Just initialize refs for the new session.
    if (prevSessionKeyRef.current !== sessionKey) {
      prevFirstMsgIdRef.current = currentFirstMsgId;
      prevScrollHeightRef.current = container.scrollHeight;
      return;
    }

    const wasPrepend =
      prevFirstMsgIdRef.current !== null &&
      prevFirstMsgIdRef.current !== currentFirstMsgId;

    if (wasPrepend) {
      const delta = container.scrollHeight - prevScrollHeightRef.current;
      if (process.env.NODE_ENV === "development") {
        console.debug("[scroll:delta] prepend detected", {
          delta,
          scrollTopBefore: container.scrollTop,
          scrollTopAfter: container.scrollTop + delta,
          scrollHeight: container.scrollHeight,
          clientHeight: container.clientHeight,
          state: stateRef.current,
          preLoadState: preLoadStateRef.current,
        });
      }
      if (delta > 0) {
        container.scrollTop += delta;
      }
      // Restore the pre-load state (e.g. "idle" if the user was browsing,
      // "pinned-bottom" if they were at the bottom).  The browser will
      // fire a scroll event from the scrollTop adjustment above; handleScroll
      // will then correct the state if the user's actual position doesn't
      // match (e.g. they scrolled during the load).
      if (stateRef.current === "loading-older") {
        transitionTo(preLoadStateRef.current);
      }
    }

    prevFirstMsgIdRef.current = currentFirstMsgId;
    prevScrollHeightRef.current = container.scrollHeight;
  }, [messageBlocks, containerRef, sessionKey, transitionTo]);

  // ── 3. Sticky-bottom (auto-scroll on new items + arrow update) ──
  //
  // When virtualCount grows (new items appended), auto-scroll to the
  // bottom IF AND ONLY IF the user is in the "pinned-bottom" state.
  //
  // This is the critical fix for the "scroll-up gets yanked back" bug:
  // the old code used `distFromBottom <= clientHeight` as the condition,
  // which meant the user had to scroll MORE than a full screen away
  // before auto-scroll disengaged.  During streaming, new messages
  // arrive every ~100ms, so the user could never scroll fast enough
  // to escape that threshold - they'd be yanked back to the bottom
  // on every single poll.
  //
  // By checking `stateRef.current === "pinned-bottom"` instead, the
  // auto-scroll disengages the moment the user scrolls up beyond
  // PIN_THRESHOLD_PX (120px), which handleScroll has already
  // translated into a transition to "idle".  The state machine is
  // the single source of truth for "should we follow the bottom?".
  //
  // Arrow buttons are still updated from the DOM read (they're purely
  // visual and don't affect scroll behavior).
  useLayoutEffect(() => {
    if (!didInitScrollRef.current) return;
    const countGrew = virtualCount > prevVirtualCountRef.current;
    prevVirtualCountRef.current = virtualCount;
    if (!countGrew || virtualCount === 0) return;

    const container = containerRef.current;
    if (!container) return;
    const distFromBottom =
      container.scrollHeight - container.scrollTop - container.clientHeight;

    // Update arrow buttons (visual only) – unified with PIN_THRESHOLD_PX
    // so the arrow appears at the exact moment the state machine leaves
    // "pinned-bottom".  This keeps the visual cue in sync with the
    // behavioral change (auto-scroll disengage, scheduleRefresh block).
    setShowScrollToBottom(distFromBottom > PIN_THRESHOLD_PX);
    setShowScrollToTop(container.scrollTop > container.clientHeight);

    // Auto-scroll ONLY if the state machine says we're pinned to the bottom.
    // This prevents yanking the user back during streaming when they've
    // intentionally scrolled up (state = "idle").
    //
    // Defense-in-depth: also verify wasAtBottomRef, which records the
    // user's actual position from the last handleScroll call (updated
    // even during loading states, before the early return).  The state
    // machine can be temporarily out of sync with the DOM (e.g.
    // handleScroll was blocked during loading, and the loading cleanup
    // restored a stale "pinned-bottom").  If wasAtBottomRef says the
    // user is NOT at the bottom, the state is stale - correct it to
    // "idle" and skip the scroll.
    //
    // We use wasAtBottomRef (not distFromBottom) because distFromBottom
    // at this point already includes the NEW content's height.  A large
    // message (e.g. code block, Mermaid diagram) would make distFromBottom
    // exceed the threshold even when the user IS at the bottom, causing
    // a false positive.  wasAtBottomRef reflects the position BEFORE
    // the new content arrived, so it correctly distinguishes "user at
    // top" from "user at bottom, large message".
    if (stateRef.current === "pinned-bottom") {
      if (wasAtBottomRef.current) {
        vmlRef.current?.scrollToBottom();
      } else {
        // State machine says pinned-bottom but the last handleScroll
        // observed the user far from the bottom.  Correct the state.
        transitionTo("idle");
      }
    }
  }, [virtualCount, containerRef, vmlRef, transitionTo]);

  // ── 4. ensureRenderable (fill viewport) ──
  //
  // If the total content height is less than the viewport height, load
  // more data to fill the viewport.  The state machine prevents this
  // from running while a load is already in progress.
  //
  // Direction: prefer loadBefore (older) when the user is in the top
  // half of the content, loadAfter (newer) when in the bottom half.
  // The old code always preferred loadAfter when state was "idle",
  // which loaded newer messages when the user was browsing at the top -
  // the wrong direction.  After loadAfter, the loading cleanup could
  // transition to "pinned-bottom" (if distFromBottom was small), causing
  // the scroll-up bounce-back bug.
  useLayoutEffect(() => {
    if (!didInitScrollRef.current) return;
    if (virtualCount === 0) return;
    if (adapter.isLoading) return;
    if (ensureRenderableCountRef.current >= MAX_ENSURE_RENDERABLE_PAGES) {
      if (process.env.NODE_ENV === "development") {
        console.debug("[scroll:ensure] blocked by counter", {
          count: ensureRenderableCountRef.current,
          max: MAX_ENSURE_RENDERABLE_PAGES,
          totalHeight: containerRef.current?.scrollHeight,
          viewportHeight: containerRef.current?.clientHeight,
        });
      }
      return;
    }

    const state = stateRef.current;
    if (state !== "idle" && state !== "pinned-bottom") return;

    const container = containerRef.current;
    if (!container) return;

    const totalHeight = container.scrollHeight;
    const viewportHeight = container.clientHeight;
    if (totalHeight >= viewportHeight) return;

    if (process.env.NODE_ENV === "development") {
      console.debug("[scroll:ensure] firing", {
        count: ensureRenderableCountRef.current + 1,
        totalHeight,
        viewportHeight,
        state,
        hasOlder: adapter.hasOlder,
        hasNewer: adapter.hasNewer,
      });
    }

    // Need more data to fill viewport.
    // Save pre-load state so scrollHeight delta and loading cleanup
    // can restore it correctly after the load completes.
    ensureRenderableCountRef.current += 1;
    preLoadStateRef.current = state;

    // Direction: use the state machine, not scroll position.  When
    // totalHeight < viewportHeight, the user can't scroll, so
    // distFromBottom is negative and pixel-based comparisons are
    // meaningless.  The state machine already knows whether the user
    // is at the bottom ("pinned-bottom") or browsing ("idle").
    if (state === "pinned-bottom" && adapter.hasNewer) {
      // User is at the bottom - load newer messages to fill from below.
      transitionTo("loading-newer");
      void adapter.loadAfter();
    } else if (adapter.hasOlder) {
      // User is browsing (idle) or at the bottom with no newer msgs -
      // load older messages to fill from above.
      transitionTo("loading-older");
      void adapter.loadBefore();
    } else if (adapter.hasNewer) {
      // No older messages available, fall back to newer.
      transitionTo("loading-newer");
      void adapter.loadAfter();
    }
  }, [virtualCount, adapter, containerRef, transitionTo]);

  // ── 5. Jump target (scroll to top/bottom after jumpToLatest/jumpToOldest) ──
  //
  // Consumes the adapter's jumpTarget (set by jumpToBottom/jumpToTop).
  // After the adapter loads the target page, this effect scrolls to the
  // appropriate end and transitions the state machine.
  useLayoutEffect(() => {
    const target = adapter.jumpTarget;
    if (!target) return;
    adapter.clearJumpTarget();

    if (target === "bottom" && virtualCount > 0) {
      vmlRef.current?.scrollToBottom();
      transitionTo("pinned-bottom");
    } else if (target === "top" && virtualCount > 0) {
      vmlRef.current?.scrollToTop();
      transitionTo("idle");
    }
  }, [adapter, virtualCount, vmlRef, transitionTo]);

  // ── 6. Loading state cleanup ──
  //
  // When isLoading becomes false, clean up any stuck loading states.
  // This handles the edge case where loadBefore/loadAfter completes but
  // no data was loaded (e.g., no older messages found), so the
  // scrollHeight delta effect didn't fire to transition the state.
  //
  // Reads the ACTUAL scroll position from the DOM - handleScroll is
  // blocked while the state is "loading-*", so it could not update
  // stateRef if the user scrolled during the load.  This microtask
  // is the only chance to set the correct state based on where the
  // user actually is.
  //
  // The transitionTo guard handles the no-overflow case automatically:
  // if scrollHeight <= clientHeight, any transitionTo("idle") is
  // redirected to "pinned-bottom", so we don't need a separate branch.
  useEffect(() => {
    if (!adapter.isLoading) {
      const state = stateRef.current;
      if (state === "loading-older" || state === "loading-newer") {
        queueMicrotask(() => {
          if (
            stateRef.current === "loading-older" ||
            stateRef.current === "loading-newer"
          ) {
            const container = containerRef.current;
            if (!container) {
              transitionTo("idle");
              return;
            }
            const distFromBottom =
              container.scrollHeight - container.scrollTop - container.clientHeight;
            if (distFromBottom <= PIN_THRESHOLD_PX) {
              transitionTo("pinned-bottom");
            } else {
              transitionTo("idle");
            }
          }
        });
      }
    }
  }, [adapter.isLoading, transitionTo]);

  // ── 7. Pagination timer ──
  //
  // The SOLE pagination trigger (besides ensureRenderable above).
  // A 150ms interval that reads scrollTop from the DOM and pagination
  // state from adapterRef (not adapter directly) so that the effect
  // has stable deps and the interval is created only once on mount.
  // The state machine prevents triggering while a load is in progress
  // (fixes Bug #2: pagination loop).
  //
  // IMPORTANT: loadAfter (newer messages) is ONLY allowed in "idle"
  // state, NEVER in "pinned-bottom".  When the user is pinned to the
  // bottom, streaming already appends the latest content - calling
  // loadAfter during streaming would fetch messages that overlap with
  // the active stream, causing duplicate content and scroll jumps.
  useEffect(() => {
    const interval = setInterval(() => {
      const container = containerRef.current;
      if (!container) return;
      const ad = adapterRef.current;
      if (ad.isLoading) return;

      const state = stateRef.current;
      if (state !== "idle" && state !== "pinned-bottom") return;

      const distFromBottom =
        container.scrollHeight - container.scrollTop - container.clientHeight;

      // ── loadBefore trigger: data-driven ──
      // Use getFirstVisibleBlockIndex() === 0 instead of scrollTop <
      // EDGE_THRESHOLD_PX.  scrollTop is unreliable because:
      //   - scrollHeight delta adjustments after prepend change it
      //   - getTotalSize() may lag behind actual measurements
      //   - measurement cache inconsistencies can offset the range
      // getFirstVisibleBlockIndex() is a pure data query: it returns
      // the index of the first block in the virtualizer's viewport.
      // If it's 0 AND hasOlder is true, the user is at the top of
      // the loaded data and we should load more.
      const firstIdx = vmlRef.current?.getFirstVisibleBlockIndex() ?? null;

      if (process.env.NODE_ENV === "development") {
        // const firstMsg = ad.blocks[0]?.items[0];
        // console.debug("[scroll:timer]", JSON.stringify({
        //   scrollTop: Math.round(container.scrollTop),
        //   scrollHeight: Math.round(container.scrollHeight),
        //   clientHeight: container.clientHeight,
        //   firstVisibleIndex: firstIdx,
        //   firstBlockType: ad.blocks[0]?.type ?? null,
        //   firstMsgType: firstMsg?.type ?? null,
        //   firstMsgId: firstMsg?.id?.slice(0, 16) ?? null,
        //   blocksCount: ad.blocks.length,
        //   hasOlder: ad.hasOlder,
        //   hasNewer: ad.hasNewer,
        //   messageOffset: ad.messageOffset,
        //   messageLimit: ad.messageLimit,
        //   messageTotal: ad.messageTotal,
        //   state,
        //   isLoading: ad.isLoading,
        // }));
      }

      if (firstIdx === 0 && ad.hasOlder) {
        if (process.env.NODE_ENV === "development") {
          console.debug("[scroll:timer] → loadBefore (firstIdx=0, hasOlder=true)");
        }
        preLoadStateRef.current = state;
        transitionTo("loading-older");
        void ad.loadBefore();
      } else if (firstIdx !== null && firstIdx !== 0 && ad.hasOlder) {
        // KEY DEBUG: hasOlder is true but firstIdx is not 0.
        // The user is at the top of the loaded data (firstIdx != 0 means
        // the first item is NOT visible), so loadBefore won't fire.
        // This is the "can't scroll to top" bug.
        if (process.env.NODE_ENV === "development") {
          // console.debug("[scroll:timer] ⚠️ hasOlder=true but firstIdx!=0", {
          //   firstIdx,
          //   hasOlder: ad.hasOlder,
          //   totalHeight: container.scrollHeight,
          //   viewportHeight: container.clientHeight,
          //   scrollTop: Math.round(container.scrollTop),
          // });
        }
      } else if (state === "idle" && distFromBottom < EDGE_THRESHOLD_PX && ad.hasNewer) {
        if (process.env.NODE_ENV === "development") {
          console.debug("[scroll:timer] → loadAfter (idle, near bottom, hasNewer=true)");
        }
        // loadAfter only in idle state - prevents streaming duplicates
        preLoadStateRef.current = state;
        transitionTo("loading-newer");
        void ad.loadAfter();
      }
    }, TIMER_INTERVAL_MS);
    return () => clearInterval(interval);
  }, [containerRef, vmlRef, transitionTo]);

  // ── 8. handleScroll (onScroll event) ──
  //
  // Called from the scroll container's onScroll event.  Updates arrow
  // button visibility, wasAtBottomRef (ground-truth position), and the
  // state machine.  wasAtBottomRef is updated BEFORE the early return
  // so it stays current even during loading/jumping states.  The state
  // machine is NOT updated during those states to prevent interference.
  const handleScroll = useCallback(() => {
    const container = containerRef.current;
    if (!container) return;

    const distFromBottom =
      container.scrollHeight - container.scrollTop - container.clientHeight;

    if (process.env.NODE_ENV === "development") {
      console.debug("[scroll:event]", JSON.stringify({
        type: "scroll",
        scrollTop: Math.round(container.scrollTop),
        scrollHeight: Math.round(container.scrollHeight),
        clientHeight: container.clientHeight,
        distFromBottom: Math.round(distFromBottom),
      }));
    }

    // Update arrow buttons – unified threshold with state machine
    setShowScrollToBottom(distFromBottom > PIN_THRESHOLD_PX);
    setShowScrollToTop(container.scrollTop > container.clientHeight);

    // Track the user's actual position on EVERY scroll event, even
    // during loading states (when the state machine is not updated).
    // This is the ground truth that §3 uses to verify the state machine.
    wasAtBottomRef.current = distFromBottom <= PIN_THRESHOLD_PX;

    // Update state machine (only if not in a loading/jumping state)
    const state = stateRef.current;
    if (state === "loading-older" || state === "loading-newer" || state === "jumping") return;

    // Reset ensureRenderable counter on every manual scroll event.
    // This prevents the "scroll stops before reaching top" bug where
    // ensureRenderableCountRef reached MAX_ENSURE_RENDERABLE_PAGES
    // during a previous fill cycle and was never reset, permanently
    // blocking ensureRenderable from re-triggering even though the
    // viewport may still be unfilled (e.g. after scrolling down and
    // back up, the scroll range is wrong and the user can't reach
    // the top to trigger the pagination timer).
    // We reset here (not just on state transitions) because the user
    // may be scrolling within the same state (e.g. idle → idle) and
    // we still want ensureRenderable to be able to re-fire.
    ensureRenderableCountRef.current = 0;

    if (distFromBottom <= PIN_THRESHOLD_PX) {
      transitionTo("pinned-bottom");
    } else {
      transitionTo("idle");
    }
  }, [containerRef, transitionTo]);

  // ── 9. User actions (arrow buttons) ──
  //
  // Sets the state machine to 'jumping' (preventing all other scroll
  // operations) and calls the adapter to load the target page.  The
  // jump target effect (section 5) will scroll to the appropriate end
  // after the data arrives.
  //
  // This replaces the old scrollToBottom/scrollToTop which had a double
  // command issue (Bug #4): both adapter.jumpToLatest() and
  // vmlRef.scrollToBottom() were called, causing conflicting scroll
  // commands.
  const jumpToBottom = useCallback(() => {
    transitionTo("jumping");
    void adapter.jumpToLatest();
  }, [adapter, transitionTo]);

  const jumpToTop = useCallback(() => {
    transitionTo("jumping");
    void adapter.jumpToOldest();
  }, [adapter, transitionTo]);

  return {
    showScrollToBottom,
    showScrollToTop,
    handleScroll,
    jumpToBottom,
    jumpToTop,
    isPinnedToBottom: () => {
      const s = stateRef.current;
      if (s === "pinned-bottom") return true;
      if (s === "loading-older" || s === "loading-newer") {
        return preLoadStateRef.current === "pinned-bottom";
      }
      return false;
    },
  };
}
