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
 *      - `showScrollToBottom` = `!isNearBottom || !adapter.isAtTail()`.
 *      - `showScrollToTop`    = `!isNearTop || adapter.hasOlder`.
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
   * Content-derived blockId of the first visible block when the user
   * left the session.  Data-driven replacement for pixel scroll offset.
   */
  initialFirstVisibleBlockId?: string | null;
  /**
   * Whether the user was at the bottom (near the latest content) when
   * they left the session.  When true, init-scroll scrolls to bottom;
   * otherwise it scrolls to initialFirstVisibleBlockId.
   */
  initialAtBottom?: boolean;
}

// ── Hook ───────────────────────────────────────────────────────────────────

export function useScrollController(config: ScrollControllerConfig): ScrollController {
  const { containerRef, adapter, vmlRef, sessionKey, initialFirstVisibleBlockId, initialAtBottom } = config;

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

  // ── Auto-follow mode ──
  // User intent: "I want to stay anchored at the latest content".
  // When true, the controller hides the "jump to latest" button and
  // forces scrollToBottom() on every liveUpdate, so streaming content
  // continuously keeps the user at the tail.  When the user actively
  // scrolls away from the bottom (> EDGE_THRESHOLD_PX), this flips to
  // false and the button reappears.
  //
  // Why useState (not useRef): drives showScrollToBottom, which gates
  // a DOM element. Re-render cost is negligible (only flips on user
  // intent, not on every streaming delta).
  const [followMode, setFollowMode] = useState(true);
  // Mirror ref so the subscribe callback (registered once, see
  // useEffect below) can read the latest followMode without re-subscribing
  // on every flip. Same pattern as adapterRef / vmlRefRef above.
  const followModeRef = useRef(followMode);
  followModeRef.current = followMode;
  // Helper: setFollowMode updates BOTH state and ref synchronously.
  // The state update drives the re-render (and thus showScrollToBottom),
  // while the ref update lets the subscribe callback — which was
  // registered ONCE on first render and reads followModeRef.current —
  // see the new value immediately, before the re-render commits.
  // Without the ref update, a liveUpdate fired in the same tick as
  // jumpToTop would still see the stale followMode and wrongly call
  // vml.scrollToBottom() (yanking the user back to the tail).
  const setFollowModeSync = useCallback((next: boolean) => {
    followModeRef.current = next;
    setFollowMode(next);
  }, []);

  // `showScrollToBottom`: show when the user is NOT viewing the latest
  // content.  Two triggers:
  //   1. User scrolled away from bottom (!isNearBottom)
  //   2. Data window doesn't cover the tail (!isAtTail)
  // Gate: NEVER show while followMode is active.  followMode is the
  // user's explicit intent to stay anchored at the tail; while it is
  // on, any divergence from the latest content is resolved by
  // auto-scrolling (see the contradiction-resolver effect below), so
  // the button would only ever appear as a stale flash.
  // Note: `adapter.hasPendingFlush()` is intentionally NOT used here.
  // A pending flush only means there are live stream entries (e.g. a
  // thinking block) that are already folded into `adapter.blocks` at the
  // tail.  Those entries are already rendered, so they do not imply the
  // user is behind the latest content.
  // Guard: hide both arrows when there are no blocks (empty session).
  // Without this, `isNearTop` defaults to `false` (set on session switch)
  // which makes `!isNearTop = true` -> `showScrollToTop = true` even though
  // there is no content to scroll.  `checkEdges()` (which updates
  // `isNearTop`) only runs after `initializedRef` is set, and
  // `initializedRef` is only set when `blocksLen > 0` - so for an empty
  // session the initial state never gets corrected.
  const hasBlocks = adapter.blocks.length > 0;
  // Raw need signal (no followMode gate): true when the data-derived
  // flags say the user is not viewing the latest content.  This is the
  // single truth source for "user needs to go to the bottom" — it drives
  // the contradiction resolver below and the arrow button.
  const needsLatestContent = hasBlocks
    && (!isNearBottom || !adapter.isAtTail());
  // Final UI flag: NEVER show the arrow while followMode is active.
  // followMode is the user's explicit intent to stay anchored at the
  // tail; while it is on, any divergence from the latest content is
  // resolved by auto-scrolling (the contradiction resolver), so the
  // button would only ever appear as a stale flash.
  const showScrollToBottom = needsLatestContent && !followMode;
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
    setFollowModeSync(true); // new session → auto-follow by default
  }, [sessionKey, setFollowModeSync]);

  // ── Init-scroll effect (useLayoutEffect — before paint) ──
  // Data-driven restoration: the saved snapshot carries a stable
  // content-derived blockId and an atBottom flag.  We never restore
  // raw pixel offsets because VML remounts with an empty per-instance
  // measurement cache; pixel offsets land in the middle of the list.
  // useLayoutEffect (not useEffect) so the scroll is applied BEFORE
  // the browser paints — no top→position flash.
  // The initializedRef guard makes it a one-shot per session.
  const blocksLen = adapter.blocks.length;
  useLayoutEffect(() => {
    if (initializedRef.current) return;
    if (blocksLen === 0) return;
    const vml = vmlRefRef.current.current;
    if (!vml) {
      initializedRef.current = true;
      return;
    }
    if (initialAtBottom) {
      // User left at the tail — show the latest content.
      vml.scrollToBottom();
    } else if (initialFirstVisibleBlockId) {
      // User was browsing - restore the first visible block by stable id.
      // If the block isn't in the loaded page (e.g. compaction removed it),
      // fall back to bottom so the user isn't stuck at an arbitrary position.
      if (!vml.scrollToBlockId(initialFirstVisibleBlockId)) {
        vml.scrollToBottom();
      }
    } else {
      // Fresh open / no useful snapshot — scroll to bottom.
      vml.scrollToBottom();
    }
    initializedRef.current = true;
  }, [blocksLen, sessionKey, initialAtBottom, initialFirstVisibleBlockId, adapter]);

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
            // Auto-follow: when followMode is active, ALWAYS force the
            // scroll to the tail on content growth.  Deliberately NO DOM
            // distance check here — in a virtualized list the scrollHeight
            // delta of an incoming stream block is not reliable within the
            // same tick (measurement hasn't run), so a distance guard
            // mis-detects "scrolled away" while the user is still visually
            // at the bottom.  That mis-detection was the bug where the
            // arrow reappeared during streaming.  Cancelling followMode is
            // owned EXCLUSIVELY by handleScroll (user scroll direction +
            // DOM distance) — see the comment there.
            if (followModeRef.current) {
              vml.scrollToBottom();
            }
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
  // Also drives followMode transitions in BOTH directions:
  //   - followMode=true  + user scroll-up past the threshold → cancel.
  //   - followMode=false + user scroll-down into the bottom zone → re-arm.
  //
  // Why direction + DOM distance (not the isNearBottom state):
  //   The old check read `isNearBottom` from state, which is one render
  //   behind inside a scroll handler.  When jumpToBottom() programmatically
  //   scrolls to the tail, that scroll fires this handler while the state
  //   still holds the PRE-scroll value (false, because the user had scrolled
  //   up before clicking the arrow) — so followMode was cancelled by our own
  //   programmatic scroll, and every subsequent liveUpdate stopped
  //   auto-scrolling.  That was the "arrow reappears after clicking it
  //   during streaming" bug.
  //   Guard #1 (direction): programmatic scrolls (jumpToBottom / the
  //   liveUpdate auto-follow) only move scrollTop DOWN (increasing), so
  //   they can never cancel followMode.  Only a user-initiated scroll-up
  //   (decreasing scrollTop) can.
  //   Guard #2 (distance): read the DOM (always current in a scroll
  //   handler) instead of state.
  //   initializedRef gate: before init-scroll the container isn't
  //   positioned yet; a session-switch clamp scroll must not cancel the
  //   fresh session's followMode.
  //
  // Why a SYMMETRIC re-arm path (scroll-down into the bottom zone):
  //   Without this, a single scroll-up → scroll-down cycle can leave
  //   followMode stuck false: scroll-up clears it, then any subsequent
  //   scroll-down event is a no-op, so new messages stop auto-scrolling
  //   even though the user is visually at the tail.  Re-arming here
  //   matches the original requirement: "when the chat box is at the
  //   bottom, followMode is true" — both the first session-open state
  //   AND any state where the user has manually returned to the tail.
  const prevScrollTopRef = useRef(0);
  const handleScroll = useCallback(() => {
    // NOTE: do NOT set initializedRef here.  When a session switches,
    // the old content is removed → scrollHeight shrinks → the browser
    // fires an async scroll event (clamping scrollTop).  This event
    // arrives AFTER the reset effect (initializedRef=false) but BEFORE
    // the new session's data loads.  Setting initializedRef=true here
    // would block the init-scroll effect from restoring the position.
    scheduleEdgeCheck();

    const container = containerRefRef.current.current;
    if (!container || !initializedRef.current) return;

    const scrollTop = container.scrollTop;
    const distFromBottom =
      container.scrollHeight - (scrollTop + container.clientHeight);
    const nearBottom = distFromBottom <= EDGE_THRESHOLD_PX;
    const scrollingUp = scrollTop < prevScrollTopRef.current;
    prevScrollTopRef.current = scrollTop;

    if (followModeRef.current) {
      // Active follow → user scroll-up past the threshold cancels it.
      // Programmatic scrolls (jumpToBottom / liveUpdate auto-follow)
      // only move scrollTop DOWN (scrollingUp=false), so they cannot
      // trip this branch — see "Guard #1" above.
      if (scrollingUp && distFromBottom > EDGE_THRESHOLD_PX) {
        setFollowModeSync(false);
      }
    } else {
      // followMode off → user scroll-down into the bottom zone re-arms
      // it.  Without this, followMode can get stuck false after a
      // scroll-up → scroll-down cycle and new messages stop
      // auto-scrolling even though the user is visually at the tail.
      // Re-arming mirrors the initial session-open default: as soon as
      // the user is "at the bottom" again, followMode becomes true.
      if (!scrollingUp && nearBottom) {
        setFollowModeSync(true);
      }
    }
  }, [scheduleEdgeCheck, setFollowModeSync]);

  // ── Jump primitives ──
  // Two-phase: (1) adapter loads data pages until the window covers
  // the target edge; (2) controller commands VML to scroll the DOM.
  // The adapter only owns data — the controller owns DOM side-effects.
  const jumpToBottom = useCallback(() => {
    // User explicitly asked to anchor at the latest content → re-enable
    // auto-follow so subsequent streaming deltas keep them pinned.
    setFollowModeSync(true);
    void adapter.scrollToBottom().then(() => {
      const vml = vmlRefRef.current.current;
      if (vml) vml.scrollToBottom();
    });
  }, [adapter, setFollowModeSync]);

  const jumpToTop = useCallback(() => {
    // User explicitly asked to jump to the oldest content → exit
    // auto-follow; the button should reappear when new messages arrive
    // (which would happen at the top, not the bottom they came from).
    setFollowModeSync(false);
    void adapter.scrollToTop().then(() => {
      const vml = vmlRefRef.current.current;
      if (vml) vml.scrollToTop();
    });
  }, [adapter, setFollowModeSync]);

  // ── Contradiction resolver (auto-follow convergence) ──
  // followMode says "keep me anchored at the latest content", but the
  // raw data-derived signal can still report the user is NOT at the
  // latest content (needsLatestContent=true) — e.g. content grew and
  // checkEdges hasn't caught up, or the tail page is still loading.
  // When the two disagree, the button must NOT appear; instead execute
  // the jump-to-bottom action directly (same code path as the arrow
  // button) so the viewport converges to the tail.  This is the single
  // source of truth: followMode=true means auto-scroll, period — never
  // a visible arrow.
  //
  // Why ref (not dep) for jumpToBottom:
  //   The effect deps are the two booleans only.  jumpToBottom's identity
  //   changes on every ChatPanel re-render because the `adapter` arg is a
  //   fresh facade object each render (see chatListAdapter.ts:628
  //   useAdapterFacade).  Including it in the deps would re-fire the
  //   effect on EVERY render while `followMode && needsLatestContent`
  //   holds true — and each fire calls adapter.scrollToBottom(), which
  //   triggers a ChatPanel re-render via the chatStore subscription,
  //   re-creating `adapter` and the `jumpToBottom` closure, and the loop
  //   repeats: "Maximum update depth exceeded" on resume-from-sleep
  //   (when many spurious re-renders happen at once).  Mirror via ref so
  //   the latest jumpToBottom is invoked without re-firing the effect.
  const jumpToBottomRef = useRef(jumpToBottom);
  jumpToBottomRef.current = jumpToBottom;

  useEffect(() => {
    if (followMode && needsLatestContent) {
      jumpToBottomRef.current();
    }
  }, [followMode, needsLatestContent]);

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