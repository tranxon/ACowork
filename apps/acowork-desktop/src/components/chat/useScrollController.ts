/**
 * useScrollController - Event-driven pagination + scroll-position
 * controller for the chat list.
 *
 * ADR-050 C5 — replaces the v1 782-line state machine with a ~150-line
 * event-driven pipeline.  The controller is the ONLY layer that reads
 * DOM (scrollTop / scrollHeight) and writes scroll-side effects.  It
 * does NOT carry any per-render scroll state — every value is derived
 * from either DOM (read on demand) or the adapter (data source).
 *
 * Adapter compatibility
 * ---------------------
 * The controller accepts ONLY the v2 `ChatListAdapterV2` (from
 * `chatListAdapter.ts`).  The v1 `useChatListAdapter.ts` is no longer
 * referenced.
 *
 * Responsibilities
 * ----------------
 * 1. **Pagination trigger** (event-driven, rAF-throttled):
 *      - On scroll events (user-initiated), check proximity to edges.
 *      - If user is near the TOP of the scroll container, call
 *        `adapter.loadPrevPage()`.
 *      - If user is near the BOTTOM, call `adapter.loadNextPage()`.
 *      - On `liveUpdate` adapter events (content growth), re-check
 *        edges to handle passive proximity (content shrank/grew).
 *      - Zero CPU when idle — no polling timer.
 *
 * 2. **Scroll arrow visibility** (data-driven, no DOM read):
 *      - `showScrollToBottom` = `!adapter.isAtTail() || adapter.hasPendingFlush()`.
 *      - `showScrollToTop`    = `adapter.hasOlder`.
 *
 * 3. **Jump buttons** (delegate to the adapter):
 *      - `jumpToBottom` = `adapter.scrollToBottom()`
 *      - `jumpToTop`    = `adapter.scrollToTop()`
 *
 * 4. **Event subscription** (C5 wired):
 *      The controller subscribes to `adapter.subscribe(...)` and on
 *      `liveUpdate` events checks whether the streaming block is
 *      in the viewport (via `vmlRef.isStreamingBlockInViewport()`).
 *      When in viewport, the browser naturally keeps the user at the
 *      bottom as scrollHeight grows.  When out of viewport, the
 *      user's reading position is preserved (no forced scroll).
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
import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import type { ChatListAdapterV2 } from "./chatListAdapter";
import type { VirtualMessageListHandle } from "./VirtualMessageList";

// ── Constants ──────────────────────────────────────────────────────────────

/**
 * Distance from the top/bottom edge of the scroll container (in px)
 * within which we trigger pagination.  Small enough that prepending /
 * appending is seamless (the user doesn't see the threshold line) and
 * large enough that a single scroll event doesn't oscillate between
 * triggering and idle.
 */
const EDGE_THRESHOLD_PX = 50;

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
  /** Whether the user is viewing the latest content (at tail, no pending). */
  isAtLatest: () => boolean;
}

export interface ScrollControllerConfig {
  /** Ref to the scroll container (owned by ChatPanel). */
  containerRef: React.RefObject<HTMLDivElement | null>;
  /** The v2 chat list adapter (single source of truth). */
  adapter: ChatListAdapterV2;
  /** Imperative handle to VirtualMessageList (scrollToBottom / scrollToTop / isStreamingBlockInViewport). */
  vmlRef: React.RefObject<VirtualMessageListHandle | null>;
  /** Session key — used to reset the tick on session change. */
  sessionKey: string | null;
  /**
   * Saved pixel scroll offset for nav-back restoration.  When undefined,
   * the init-scroll effect scrolls to bottom on first data arrival.
   * When defined, the init-scroll useLayoutEffect sets
   * `container.scrollTop = initialScrollOffset` synchronously (before
   * paint) to restore the user's reading position.  The virtualizer's
   * `initialOffset` option alone does NOT set the DOM scrollTop — it
   * only seeds the virtualizer's internal scroll-offset state.
   */
  initialScrollOffset?: number;
  /**
   * Absolute message index the user was viewing when they left the
   * session.  When set, the init-scroll effect uses the adapter's
   * scrollToPosition (index-based, virtualizer-aware) instead of raw
   * pixel scrollTop — this is reliable across session switches where
   * the loaded page (and thus scrollHeight) may differ.
   * Takes priority over initialScrollOffset when both are set.
   */
  initialMessageIndex?: number;
}

// ── Hook ───────────────────────────────────────────────────────────────────

export function useScrollController(config: ScrollControllerConfig): ScrollController {
  const { containerRef, adapter, vmlRef, sessionKey, initialScrollOffset, initialMessageIndex } = config;

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

  // ── Scroll-position tracking ──
  // Two boolean states track whether the viewport is near each edge.
  // They flip when the user crosses the EDGE_THRESHOLD_PX boundary,
  // driving a minimal re-render to update arrow-button visibility.
  // Without these, button visibility is purely data-derived and misses
  // the case where the user scrolls within a page that's already at
  // the data-window tail/head.
  const [isNearBottom, setIsNearBottom] = useState(true);
  const [isNearTop, setIsNearTop] = useState(false);

  // `showScrollToBottom`: show when the user is NOT viewing the latest
  // content.  Three triggers:
  //   1. User scrolled away from bottom (!isNearBottom)
  //   2. Data window doesn't cover the tail (!isAtTail)
  //   3. Live content waiting to be flushed (hasPendingFlush)
  // Guard: hide both arrows when there are no blocks (empty session).
  // Without this, `isNearTop` defaults to `false` (set on session switch)
  // which makes `!isNearTop = true` -> `showScrollToTop = true` even though
  // there is no content to scroll.  `checkEdges()` (which updates
  // `isNearTop`) only runs after `initializedRef` is set, and
  // `initializedRef` is only set when `blocksLen > 0` - so for an empty
  // session the initial state never gets corrected.
  const hasBlocks = adapter.blocks.length > 0;
  const showScrollToBottom = hasBlocks
    && (!isNearBottom || !adapter.isAtTail() || adapter.hasPendingFlush());
  const showScrollToTop = hasBlocks
    && (!isNearTop || adapter.hasOlder);

  // ── Initial positioning gate ──
  // Prevents edge-detection from triggering loadPrevPage before the
  // scroll container has been positioned (scroll-to-bottom on fresh
  // open, or restored via initialOffset).  Without this gate, the
  // first scroll event reads scrollTop=0 (the virtualizer's
  // initialOffset) and mistakes it for "user scrolled to top" —
  // cascading loadPrevPage until all messages are loaded.
  const initializedRef = useRef(false);

  // Reset on session switch.  Also reset prepend-tracking refs so the
  // scroll-preservation effect doesn't misfire across sessions.
  // MUST be useLayoutEffect (not useEffect) so it fires BEFORE the
  // init-scroll useLayoutEffect below — both are useLayoutEffect and
  // React fires them in definition order.  If this were useEffect it
  // would fire AFTER the init-scroll, leaving initializedRef=true
  // when the init-scroll checks it (skipping the scroll restoration).
  useLayoutEffect(() => {
    initializedRef.current = false;
    setIsNearBottom(true); // fresh session starts at bottom
    setIsNearTop(false);   // ...not at top
  }, [sessionKey]);

  // ── Init-scroll effect (useLayoutEffect — before paint) ──
  // When blocks first arrive for a session, position the container:
  //   - No saved offset (initialScrollOffset === undefined): scroll to
  //     bottom — the user expects to see the latest messages.
  //   - Saved offset provided: set container.scrollTop to the saved
  //     pixel offset.  The virtualizer's `initialOffset` option only
  //     seeds its INTERNAL scroll-offset state; it does NOT write the
  //     DOM element's scrollTop.  Without this explicit assignment the
  //     container starts at scrollTop=0 and the user sees the top of
  //     the page instead of their saved reading position.
  // useLayoutEffect (not useEffect) so the scroll is applied BEFORE
  // the browser paints — no top→position flash.
  // The initializedRef guard makes it a one-shot per session.
  const blocksLen = adapter.blocks.length;
  useLayoutEffect(() => {
    if (initializedRef.current) return;
    if (blocksLen === 0) return;
    console.warn("[SC:init-scroll]", {
      sessionKey,
      blocksLen,
      initialMessageIndex,
      initialScrollOffset,
      messageOffset: adapter.messageOffset,
    });
    if (initialMessageIndex !== undefined) {
      // Content-based restoration: compute the block index relative to
      // the currently-loaded window and delegate to the adapter's
      // scrollToPosition (which emits scrollToIndex → vml.scrollToIndex).
      // This is reliable regardless of scrollHeight differences between
      // the saving and restoring sessions.
      const relativeIndex = Math.max(0, Math.min(
        initialMessageIndex - adapter.messageOffset,
        blocksLen - 1,
      ));
      console.warn("[SC:init-scroll] content-based →", { relativeIndex, initialMessageIndex, adapterOffset: adapter.messageOffset });
      adapter.scrollToPosition(relativeIndex);
    } else if (initialScrollOffset !== undefined) {
      // Pixel-based fallback (nav-back within same mount, no remount).
      const container = containerRefRef.current.current;
      console.warn("[SC:init-scroll] pixel-based →", { initialScrollOffset });
      if (container) {
        container.scrollTop = initialScrollOffset;
      }
    } else {
      // Fresh open / sending — scroll to bottom.
      console.warn("[SC:init-scroll] scroll-to-bottom (no snapshot)");
      const vml = vmlRefRef.current.current;
      if (vml) {
        vml.scrollToBottom();
      }
    }
    // Whether we scrolled or relied on initialOffset, mark done.
    initializedRef.current = true;
  }, [blocksLen, sessionKey, initialScrollOffset, initialMessageIndex, adapter]);

  // ── Scroll preservation on prepend (useLayoutEffect) ──
  // When loadPrevPage() prepends older messages, the virtualizer's
  // totalSize grows but the browser keeps scrollTop unchanged.  Without
  // correction the user's viewport shows the SAME pixels — the newly
  // loaded content is above the fold and invisible.  The user perceives
  // "scrolling doesn't work" because repeated scroll-up → loadPrevPage
  // cycles produce no visible change.
  //
  // Fix: in a useLayoutEffect (runs after DOM commit, before paint),
  // detect prepend (first blockId changed + block count grew) and
  // shift scrollTop by the scrollHeight delta.  This keeps the user's
  // viewport anchored to the same content while new content appears
  // above — the standard chat-app "infinite scroll up" behavior.
  //
  // Detection uses blockId (content-derived, stable) rather than
  // messageOffset to avoid false positives when a session reload
  // resets offset to 0.
  const firstBlockId = blocksLen > 0 ? adapter.blocks[0].blockId : null;
  const cacheGeneration = adapter.cacheGeneration;
  const prevBlocksLenRef = useRef(0);
  const prevFirstBlockIdRef = useRef<string | null>(null);
  const prevScrollHeightRef = useRef(0);
  const prevSessionKeyForPrependRef = useRef<string | null | undefined>(undefined);
  const prevCacheGenRef = useRef(cacheGeneration);

  // Capture scrollHeight BEFORE the DOM commit (during render) so the
  // useLayoutEffect can compute the delta after the commit.
  const containerNow = containerRef.current;
  if (containerNow) {
    prevScrollHeightRef.current = containerNow.scrollHeight;
  }

  useLayoutEffect(() => {
    const container = containerRefRef.current.current;

    // Session changed — reset tracking refs, skip adjustment.
    if (prevSessionKeyForPrependRef.current !== sessionKey) {
      prevSessionKeyForPrependRef.current = sessionKey;
      prevBlocksLenRef.current = blocksLen;
      prevFirstBlockIdRef.current = firstBlockId;
      prevCacheGenRef.current = cacheGeneration;
      if (container) prevScrollHeightRef.current = container.scrollHeight;
      return;
    }

    // Cache replaced (jump operation) — NOT a prepend.  Reset refs
    // and skip anchoring; the caller (scrollToTop/Bottom/Position)
    // owns the subsequent scroll placement.
    if (prevCacheGenRef.current !== cacheGeneration) {
      prevCacheGenRef.current = cacheGeneration;
      prevBlocksLenRef.current = blocksLen;
      prevFirstBlockIdRef.current = firstBlockId;
      if (container) prevScrollHeightRef.current = container.scrollHeight;
      return;
    }

    const prevLen = prevBlocksLenRef.current;
    const prevId = prevFirstBlockIdRef.current;
    prevBlocksLenRef.current = blocksLen;
    prevFirstBlockIdRef.current = firstBlockId;

    if (!container) return;
    // Guard: only adjust on genuine prepend within the same session.
    // - prevId !== null: not the first data arrival (initial load).
    // - blocksLen > prevLen: content grew (not a clear / replace).
    // - firstBlockId changed: the head of the list is different
    //   (prepend), not just an append or in-place update.
    if (prevId === null || blocksLen <= prevLen || firstBlockId === prevId) {
      prevScrollHeightRef.current = container.scrollHeight;
      return;
    }

    // Prepend detected — shift scrollTop by the height delta so the
    // user's viewport stays anchored to the same content.
    const delta = container.scrollHeight - prevScrollHeightRef.current;
    if (delta > 0) {
      container.scrollTop += delta;
    }
    prevScrollHeightRef.current = container.scrollHeight;
  }, [blocksLen, firstBlockId, sessionKey, cacheGeneration]);

  // ── Edge-detection (shared by scroll handler + adapter events) ──
  // Reads DOM scrollTop/scrollHeight and triggers pagination when the
  // user is within EDGE_THRESHOLD_PX of either edge.  Called from:
  //   1. handleScroll (user-initiated scroll, rAF-throttled)
  //   2. adapter liveUpdate event (content growth may passively push
  //      the viewport near an edge)
  // Gated by initializedRef and adapter.isLoading.
  // Also updates `isNearBottom` state for arrow-button visibility.
  const checkEdges = useCallback(() => {
    if (!initializedRef.current) return;
    const container = containerRefRef.current.current;
    const a = adapterRef.current;
    if (!container) return;

    // ── Update scroll-position state (drives arrow buttons) ──
    const scrollTop = container.scrollTop;
    const distFromBottom =
      container.scrollHeight - (scrollTop + container.clientHeight);
    const nearBottom = distFromBottom <= EDGE_THRESHOLD_PX;
    const nearTop = scrollTop <= EDGE_THRESHOLD_PX;
    setIsNearBottom((prev) => (prev === nearBottom ? prev : nearBottom));
    setIsNearTop((prev) => (prev === nearTop ? prev : nearTop));

    // ── Pagination trigger ──
    if (!a) return;
    if (a.isLoading) return;
    // near-top → load older
    if (scrollTop <= EDGE_THRESHOLD_PX && a.hasOlder) {
      void a.loadPrevPage();
      return;
    }
    // near-bottom → load newer
    if (distFromBottom <= EDGE_THRESHOLD_PX && a.hasNewer) {
      void a.loadNextPage();
    }
  }, []);

  // rAF gate — coalesces multiple scroll events within a single frame
  // into one checkEdges call.  Prevents layout thrashing on high-freq
  // scroll events (trackpad / 120Hz displays can fire 120+ events/s).
  const rafPendingRef = useRef(false);
  const scheduleEdgeCheck = useCallback(() => {
    if (rafPendingRef.current) return;
    rafPendingRef.current = true;
    requestAnimationFrame(() => {
      rafPendingRef.current = false;
      checkEdges();
    });
  }, [checkEdges]);

  // ── Event subscription ──
  // The controller subscribes to adapter events:
  //   - `liveUpdate`: content grew (stream delta / record complete).
  //     Re-check edges — content growth may passively push the viewport
  //     within EDGE_THRESHOLD_PX of an edge (e.g. total shrank after
  //     compaction, or user was already near bottom and new content
  //     arrived).  Also runs the streaming-block viewport diagnostic.
  //   - `pageLoaded`: a page was loaded (prepend/append).  Re-check
  //     edges in case the new content didn't push the viewport far
  //     enough from the threshold (rapid consecutive scrolls).
  //   - `scrollToIndex`: adapter.scrollToPosition() was called.  The
  //     controller executes the DOM scroll via vmlRef.scrollToIndex().
  useEffect(() => {
    const unsub = adapter.subscribe((event) => {
      switch (event.type) {
        case "liveUpdate": {
          const vml = vmlRefRef.current.current;
          if (vml) {
            void vml.isStreamingBlockInViewport();
          }
          // Content grew — passive edge proximity may have changed.
          scheduleEdgeCheck();
          break;
        }
        case "pageLoaded":
          // Page loaded — re-check in case user is still at edge.
          scheduleEdgeCheck();
          break;
        case "scrollToIndex": {
          // adapter.scrollToPosition() requests a DOM scroll to a
          // specific block index.  The controller owns DOM side-effects.
          const vml = vmlRefRef.current.current;
          if (vml) {
            vml.scrollToIndex(event.index);
          }
          break;
        }
        default:
          break;
      }
    });
    return unsub;
  }, [adapter, sessionKey, scheduleEdgeCheck]);

  // ── handleScroll ──
  // Called from the scroll container's onScroll event (bound in
  // ChatPanel).  This is the PRIMARY pagination trigger — event-driven,
  // rAF-throttled.  Zero CPU when the user is not scrolling.
  const handleScroll = useCallback(() => {
    // NOTE: do NOT set initializedRef here.  When a session switches,
    // the old content is removed → scrollHeight shrinks → the browser
    // fires an async scroll event (clamping scrollTop).  This event
    // arrives AFTER the reset effect (initializedRef=false) but BEFORE
    // the new session's data loads.  Setting initializedRef=true here
    // would block the init-scroll effect from restoring the position.
    scheduleEdgeCheck();
  }, [scheduleEdgeCheck]);

  // ── Jump primitives ──
  // Two-phase: (1) adapter loads data pages until the window covers
  // the target edge; (2) controller commands VML to scroll the DOM.
  // The adapter only owns data — the controller owns DOM side-effects.
  const jumpToBottom = useCallback(() => {
    void adapter.scrollToBottom().then(() => {
      const vml = vmlRefRef.current.current;
      if (vml) vml.scrollToBottom();
    });
  }, [adapter]);

  const jumpToTop = useCallback(() => {
    void adapter.scrollToTop().then(() => {
      const vml = vmlRefRef.current.current;
      if (vml) vml.scrollToTop();
    });
  }, [adapter]);

  // ── isAtLatest ──
  // Preserved for ChatPanel's scroll-snapshot logic (nav-back restore).
  // Pure data derivation — no DOM read, no stored state.  ADR-050:
  // replaces the removed `isPinnedToBottom` stored field with a
  // computed query on the adapter.
  const isAtLatest = useCallback((): boolean => {
    return adapter.isAtTail() && !adapter.hasPendingFlush();
  }, [adapter]);

  return {
    showScrollToBottom,
    showScrollToTop,
    handleScroll,
    jumpToBottom,
    jumpToTop,
    isAtLatest,
  };
}