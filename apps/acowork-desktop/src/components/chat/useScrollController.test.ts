/**
 * useScrollController — unit tests for the C5 event-driven controller.
 *
 * ADR-050 §7 C5 verification:
 *   - tsc --noEmit: clean (smoke-checked outside)
 *   - Manual tests in §8 acceptance matrix
 *   - Unit tests pin the data-derived flag contract (the only piece
 *     that doesn't need a DOM):
 *       showScrollToBottom = !isNearBottom || !adapter.isAtTail()
 *       showScrollToTop    = !isNearTop || adapter.hasOlder
 *       isAtLatest   = adapter.isAtTail() && !adapter.hasPendingFlush()
 *       jumpToBottom / jumpToTop  delegate to adapter
 *
 * The event-driven pagination trigger (checkEdges via rAF) reads DOM
 * and is covered indirectly: any provider that toggles `isLoading`
 * correctly would dedupe concurrent loads, so we don't need to drive
 * scroll events in unit tests.
 */
import { describe, expect, it, vi } from "vitest";
import { act, renderHook } from "@testing-library/react";
import { useScrollController } from "./useScrollController";
import type { ChatListAdapterV2 } from "./chatListAdapter";
import type { MessageBlock } from "./messageFolder";

const DUMMY_BLOCK: MessageBlock = {
  blockId: "block-1",
  type: "user",
  items: [],
  rawCount: 1,
  anchorToLatest: false,
  hasFollowUpReply: false,
  isLive: false,
};

function makeMockAdapter(overrides: Partial<ChatListAdapterV2> = {}): ChatListAdapterV2 {
  return {
    blocks: [DUMMY_BLOCK],
    totalBlocks: 1,
    messageOffset: 0,
    messageLimit: 0,
    messageTotal: 0,
    isAtTail: () => true,
    hasPendingFlush: () => false,
    hasOlder: false,
    hasNewer: false,
    isLoading: false,
    loadPrevPage: vi.fn(async () => {}),
    loadNextPage: vi.fn(async () => {}),
    scrollToTop: vi.fn(async () => {}),
    scrollToBottom: vi.fn(async () => {}),
    scrollToPosition: vi.fn(async () => {}),
    subscribe: vi.fn(() => () => {}),
    ...overrides,
  };
}

describe("useScrollController: data-derived flags", () => {
  it("showScrollToBottom=false when not at tail but followMode active (streaming case)", () => {
    // Default state: followMode=true (fresh session starts in follow
    // mode) and isNearBottom=true.  Even when isAtTail() is false (data
    // window hasn't caught up to the live stream), the button stays
    // hidden — the controller will auto-scroll on each liveUpdate.
    const adapter = makeMockAdapter({ isAtTail: () => false });
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    expect(result.current.showScrollToBottom).toBe(false);
  });

  it("showScrollToBottom=true when not at tail and followMode disabled", () => {
    // Once followMode is off, the original isAtTail check applies.
    const adapter = makeMockAdapter({ isAtTail: () => false });
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    // jumpToTop exits follow mode (act() flushes the state update so
    // result.current reflects the post-render value).
    act(() => {
      result.current.jumpToTop();
    });
    expect(result.current.showScrollToBottom).toBe(true);
  });

  it("showScrollToBottom=false when at tail and no pending flush", () => {
    const adapter = makeMockAdapter({ isAtTail: () => true, hasPendingFlush: () => false });
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    expect(result.current.showScrollToBottom).toBe(false);
  });

  it("showScrollToBottom=false when at tail with pending flush but near bottom", () => {
    const adapter = makeMockAdapter({ isAtTail: () => true, hasPendingFlush: () => true });
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    expect(result.current.showScrollToBottom).toBe(false);
  });

  it("showScrollToBottom=false and showScrollToTop=false when no blocks (empty session)", () => {
    const adapter = makeMockAdapter({ blocks: [], totalBlocks: 0 });
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    expect(result.current.showScrollToBottom).toBe(false);
    expect(result.current.showScrollToTop).toBe(false);
  });

  it("showScrollToTop: hasOlder=true always shows; hasOlder=false depends on isNearTop", () => {
    // hasOlder=true → always show (can load older pages)
    const adapter1 = makeMockAdapter({ hasOlder: true });
    const { result: r1 } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter: adapter1,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    expect(r1.current.showScrollToTop).toBe(true);

    // hasOlder=false + initial isNearTop=false (not at top) → show
    // (user is mid-list, can scroll up within current page)
    const adapter2 = makeMockAdapter({ hasOlder: false });
    const { result: r2 } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter: adapter2,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    // isNearTop defaults to false → !isNearTop=true → showScrollToTop=true
    expect(r2.current.showScrollToTop).toBe(true);
  });

  it("isAtLatest() returns true when at tail and no pending flush", () => {
    const adapter = makeMockAdapter({ isAtTail: () => true, hasPendingFlush: () => false });
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    expect(result.current.isAtLatest()).toBe(true);
  });

  it("isAtLatest() returns false when not at tail", () => {
    const adapter = makeMockAdapter({ isAtTail: () => false });
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    expect(result.current.isAtLatest()).toBe(false);
  });

  it("isAtLatest() returns false when pending flush (streaming not yet caught up)", () => {
    const adapter = makeMockAdapter({ isAtTail: () => true, hasPendingFlush: () => true });
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    expect(result.current.isAtLatest()).toBe(false);
  });
});

describe("useScrollController: jump primitives delegate to adapter", () => {
  it("jumpToBottom calls adapter.scrollToBottom", () => {
    const scrollToBottom = vi.fn(async () => {});
    const adapter = makeMockAdapter({ scrollToBottom });
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    act(() => {
      result.current.jumpToBottom();
    });
    expect(scrollToBottom).toHaveBeenCalledTimes(1);
  });

  it("jumpToTop calls adapter.scrollToTop", () => {
    const scrollToTop = vi.fn(async () => {});
    const adapter = makeMockAdapter({ scrollToTop });
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    act(() => {
      result.current.jumpToTop();
    });
    expect(scrollToTop).toHaveBeenCalledTimes(1);
  });
});

describe("useScrollController: subscription", () => {
  it("subscribes to adapter on mount, unsubscribes on unmount", () => {
    const unsub = vi.fn();
    const subscribe = vi.fn(() => unsub);
    const adapter = makeMockAdapter({ subscribe });
    const { unmount } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    expect(subscribe).toHaveBeenCalledTimes(1);
    unmount();
    expect(unsub).toHaveBeenCalledTimes(1);
  });

  it("re-subscribes when sessionKey changes", () => {
    const unsub = vi.fn();
    const subscribe = vi.fn(() => unsub);
    const adapter = makeMockAdapter({ subscribe });
    const { rerender } = renderHook(
      ({ sessionKey }) =>
        useScrollController({
          containerRef: { current: null },
          adapter,
          vmlRef: { current: null },
          sessionKey,
        }),
      { initialProps: { sessionKey: "agent:sess1" } },
    );
    expect(subscribe).toHaveBeenCalledTimes(1);
    expect(unsub).toHaveBeenCalledTimes(0);
    rerender({ sessionKey: "agent:sess2" });
    expect(unsub).toHaveBeenCalledTimes(1);
    expect(subscribe).toHaveBeenCalledTimes(2);
  });
});

describe("useScrollController: data-driven init scroll", () => {
  // Helper: build a vmlRef with the imperative methods the controller calls.
  function makeMockVml() {
    return {
      current: {
        scrollToBottom: vi.fn(),
        scrollToTop: vi.fn(),
        scrollToIndex: vi.fn(),
        scrollToBlockId: vi.fn(() => true),
        getFirstVisibleBlockIndex: vi.fn(() => null),
        getFirstVisibleBlockId: vi.fn(() => null),
        getLastVisibleBlockIndex: vi.fn(() => null),
        isStreamingBlockInViewport: vi.fn(() => false),
      } as any,
    };
  }

  it("calls scrollToBottom when initialAtBottom=true", () => {
    const vml = makeMockVml();
    const adapter = makeMockAdapter();
    renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: vml,
        sessionKey: "agent:sess",
        initialAtBottom: true,
        initialFirstVisibleBlockId: "block-msg-99",
      }),
    );
    expect(vml.current.scrollToBottom).toHaveBeenCalledTimes(1);
    expect(vml.current.scrollToBlockId).not.toHaveBeenCalled();
  });

  it("calls scrollToBlockId when initialAtBottom=false and blockId is set", () => {
    const vml = makeMockVml();
    const adapter = makeMockAdapter();
    renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: vml,
        sessionKey: "agent:sess",
        initialAtBottom: false,
        initialFirstVisibleBlockId: "block-msg-30",
      }),
    );
    expect(vml.current.scrollToBlockId).toHaveBeenCalledWith("block-msg-30");
    expect(vml.current.scrollToBottom).not.toHaveBeenCalled();
  });

  it("falls back to scrollToBottom when atBottom=false and no blockId", () => {
    const vml = makeMockVml();
    const adapter = makeMockAdapter();
    renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: vml,
        sessionKey: "agent:sess",
        initialAtBottom: false,
        initialFirstVisibleBlockId: null,
      }),
    );
    expect(vml.current.scrollToBottom).toHaveBeenCalledTimes(1);
    expect(vml.current.scrollToBlockId).not.toHaveBeenCalled();
  });

  it("falls back to scrollToBottom when no initial props (fresh open)", () => {
    const vml = makeMockVml();
    const adapter = makeMockAdapter();
    renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: vml,
        sessionKey: "agent:sess",
      }),
    );
    expect(vml.current.scrollToBottom).toHaveBeenCalledTimes(1);
  });

  it("does not re-run init scroll when sessionKey changes within an already-initialized session", () => {
    // The reset effect resets initializedRef on sessionKey change, so the
    // next render with new blocks will run init scroll once for the new
    // session.  We verify that within the same session, a re-render with
    // the same blocksLen does not call scrollToBlockId a second time.
    const vml = makeMockVml();
    const adapter = makeMockAdapter();
    const { rerender } = renderHook(
      ({ sessionKey }) =>
        useScrollController({
          containerRef: { current: null },
          adapter,
          vmlRef: vml,
          sessionKey,
          initialAtBottom: false,
          initialFirstVisibleBlockId: "block-msg-30",
        }),
      { initialProps: { sessionKey: "agent:sess" } },
    );
    expect(vml.current.scrollToBlockId).toHaveBeenCalledTimes(1);
    rerender({ sessionKey: "agent:sess" });
    expect(vml.current.scrollToBlockId).toHaveBeenCalledTimes(1);
  });

  it("falls back to scrollToBottom when scrollToBlockId returns false (block not found)", () => {
    const vml = makeMockVml();
    // Override: scrollToBlockId returns false (block not in loaded page).
    (vml.current.scrollToBlockId as ReturnType<typeof vi.fn>).mockReturnValue(false);
    const adapter = makeMockAdapter();
    renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: vml,
        sessionKey: "agent:sess",
        initialAtBottom: false,
        initialFirstVisibleBlockId: "block-msg-999",
      }),
    );
    expect(vml.current.scrollToBlockId).toHaveBeenCalledWith("block-msg-999");
    expect(vml.current.scrollToBottom).toHaveBeenCalledTimes(1);
  });
});

describe("useScrollController: followMode auto-follow", () => {
  // Helper: build a vmlRef with scrollToBottom + isStreamingBlockInViewport
  // (the two methods the controller invokes during liveUpdate auto-follow).
  function makeAutoFollowVml() {
    return {
      current: {
        scrollToBottom: vi.fn(),
        scrollToTop: vi.fn(),
        scrollToIndex: vi.fn(),
        scrollToBlockId: vi.fn(() => true),
        getFirstVisibleBlockIndex: vi.fn(() => null),
        getFirstVisibleBlockId: vi.fn(() => null),
        getLastVisibleBlockIndex: vi.fn(() => null),
        isStreamingBlockInViewport: vi.fn(() => false),
      } as any,
    };
  }

  // Helper: build a subscribe mock that captures the callback so we can
  // fire events at will after the useEffect has run.  Synchronously
  // firing inside subscribe() runs DURING render and before effects,
  // which would make init-scroll calls indistinguishable from
  // liveUpdate-triggered calls in the assertions.
  function makeDeferredSubscribe(): {
    subscribe: ReturnType<typeof vi.fn>;
    fire: (event: { type: string }) => void;
  } {
    let cb: ((event: any) => void) | null = null;
    const subscribe = vi.fn((fn: (event: any) => void) => {
      cb = fn;
      return () => {};
    });
    return {
      subscribe,
      fire: (event) => {
        if (!cb) throw new Error("subscribe not yet called");
        cb(event);
      },
    };
  }

  // ── showScrollToBottom behavior ────────────────────────────────────────

  it("fresh session: button hidden even though isAtTail=false (streaming)", () => {
    // Default state — followMode=true (session-reset effect), isNearBottom=true.
    // isAtTail=false simulates pending streaming content not yet in cache.
    const adapter = makeMockAdapter({ isAtTail: () => false });
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    expect(result.current.showScrollToBottom).toBe(false);
  });

  it("jumpToBottom re-enables followMode → button hidden again", () => {
    const scrollToBottom = vi.fn(async () => {});
    const adapter = makeMockAdapter({ isAtTail: () => false, scrollToBottom });
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    // Mount effect (contradiction resolver: isAtTail=false + followMode=true)
    // already fired jumpToBottom once — clear it so we only count the
    // explicit jumpToTop/jumpToBottom sequence below.
    scrollToBottom.mockClear();

    // Exit follow mode via jumpToTop
    act(() => {
      result.current.jumpToTop();
    });
    expect(result.current.showScrollToBottom).toBe(true);

    // Click the button → re-enter follow mode.  The explicit jumpToBottom
    // (plus the contradiction resolver re-running on the followMode flip)
    // triggers at least one adapter scroll.
    act(() => {
      result.current.jumpToBottom();
    });
    expect(scrollToBottom).toHaveBeenCalled();
    expect(result.current.showScrollToBottom).toBe(false);
  });

  it("jumpToTop exits followMode", () => {
    const adapter = makeMockAdapter({ isAtTail: () => false });
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    // Default: button hidden (followMode=true, near bottom)
    expect(result.current.showScrollToBottom).toBe(false);

    act(() => {
      result.current.jumpToTop();
    });
    expect(result.current.showScrollToBottom).toBe(true);
  });

  it("session switch resets followMode to true", () => {
    const adapter = makeMockAdapter({ isAtTail: () => false });
    const { result, rerender } = renderHook(
      ({ sessionKey }) =>
        useScrollController({
          containerRef: { current: null },
          adapter,
          vmlRef: { current: null },
          sessionKey,
        }),
      { initialProps: { sessionKey: "agent:sess1" } },
    );

    // Exit follow mode
    act(() => {
      result.current.jumpToTop();
    });
    expect(result.current.showScrollToBottom).toBe(true);

    // Switch session → followMode resets to true (rerender flushes the
    // sessionKey change effect).
    rerender({ sessionKey: "agent:sess2" });
    expect(result.current.showScrollToBottom).toBe(false);
  });

  // ── liveUpdate auto-scroll behavior ────────────────────────────────────

  it("liveUpdate forces scrollToBottom when followMode active and near bottom", async () => {
    const vml = makeAutoFollowVml();
    const { subscribe, fire } = makeDeferredSubscribe();
    const adapter = makeMockAdapter({ subscribe });
    renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: vml,
        sessionKey: "agent:sess",
      }),
    );

    // Init-scroll calls scrollToBottom once — clear it so we only count
    // liveUpdate-driven calls.
    vml.current.scrollToBottom.mockClear();

    // Fire liveUpdate AFTER the useEffect has registered (wait one tick).
    await Promise.resolve();
    fire({ type: "liveUpdate" });

    expect(vml.current.scrollToBottom).toHaveBeenCalledTimes(1);
  });

  it("liveUpdate does NOT auto-scroll when followMode disabled", async () => {
    const vml = makeAutoFollowVml();
    const { subscribe, fire } = makeDeferredSubscribe();
    const adapter = makeMockAdapter({ subscribe, isAtTail: () => false });
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: vml,
        sessionKey: "agent:sess",
      }),
    );

    // Mount effect (contradiction resolver) fires jumpToBottom once because
    // isAtTail=false + followMode=true.  Flush its .then chain first, then
    // exit follow mode.
    await Promise.resolve();
    act(() => {
      result.current.jumpToTop();
    });
    vml.current.scrollToBottom.mockClear();

    fire({ type: "liveUpdate" });

    expect(vml.current.scrollToBottom).not.toHaveBeenCalled();
  });

  it("liveUpdate does NOT auto-scroll after the user scrolls away (followMode cancelled)", async () => {
    const vml = makeAutoFollowVml();
    const { subscribe, fire } = makeDeferredSubscribe();
    // Container reports a large distance from bottom (> EDGE_THRESHOLD_PX).
    const container = {
      scrollTop: 500,
      scrollHeight: 2000,
      clientHeight: 400, // distFromBottom = 2000-500-400 = 1100 > 50
    } as unknown as HTMLDivElement;
    const adapter = makeMockAdapter({ subscribe });
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: container },
        adapter,
        vmlRef: vml,
        sessionKey: "agent:sess",
      }),
    );

    // User scrolls away: baseline scroll-down, then a scroll-up past the
    // threshold.  The scroll-up cancels followMode.
    act(() => {
      result.current.handleScroll(); // 500 > prev 0 → down, no cancel
    });
    container.scrollTop = 100;
    act(() => {
      result.current.handleScroll(); // 100 < 500 → up, dist 1500 > 50 → cancel
    });

    vml.current.scrollToBottom.mockClear();

    fire({ type: "liveUpdate" });

    expect(vml.current.scrollToBottom).not.toHaveBeenCalled();
  });

  it("programmatic scroll-down does not cancel followMode", () => {
    // jumpToBottom's programmatic scroll fires a scroll event while the
    // container is already at (or moving to) the bottom.  Moving DOWN must
    // never cancel followMode — this was the bug where clicking the arrow
    // during streaming made the arrow reappear.
    const adapter = makeMockAdapter();
    const container = {
      scrollTop: 1600,
      scrollHeight: 2000,
      clientHeight: 400, // distFromBottom = 0 ≤ threshold
    } as unknown as HTMLDivElement;
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: container },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );

    act(() => {
      result.current.handleScroll();
    });

    expect(result.current.showScrollToBottom).toBe(false);
  });

  it("user scroll-up away from bottom cancels followMode (arrow reappears)", () => {
    const adapter = makeMockAdapter({ isAtTail: () => false });
    const container = {
      scrollTop: 500,
      scrollHeight: 2000,
      clientHeight: 400,
    } as unknown as HTMLDivElement;
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: container },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );

    // followMode defaults to true → arrow hidden even though !isAtTail.
    expect(result.current.showScrollToBottom).toBe(false);

    // Baseline scroll-down, then scroll-up past the threshold.
    act(() => {
      result.current.handleScroll(); // 500 > prev 0 → down
    });
    container.scrollTop = 100;
    act(() => {
      result.current.handleScroll(); // 100 < 500 → up, dist 1500 > 50 → cancel
    });

    expect(result.current.showScrollToBottom).toBe(true);
  });

  it("bug repro: click arrow → programmatic scroll → liveUpdate keeps auto-scrolling", async () => {
    const vml = makeAutoFollowVml();
    const { subscribe, fire } = makeDeferredSubscribe();
    // isAtTail=false keeps needsLatestContent constant so the assertions
    // don't depend on rAF-driven isNearBottom state updates (which the
    // test environment never runs).
    const adapter = makeMockAdapter({ subscribe, isAtTail: () => false });
    const container = {
      scrollTop: 500,
      scrollHeight: 2000,
      clientHeight: 400,
    } as unknown as HTMLDivElement;
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: container },
        adapter,
        vmlRef: vml,
        sessionKey: "agent:sess",
      }),
    );

    // Mount effect (contradiction resolver: isAtTail=false + followMode=true)
    // fires jumpToBottom once — flush its .then chain and ignore it.
    await Promise.resolve();
    vml.current.scrollToBottom.mockClear();

    // User had scrolled up → followMode cancelled → arrow visible.
    act(() => {
      result.current.handleScroll(); // down baseline
    });
    container.scrollTop = 100;
    act(() => {
      result.current.handleScroll(); // up → cancel
    });
    expect(result.current.showScrollToBottom).toBe(true);

    // Click the arrow → jumpToBottom → followMode re-enabled.
    act(() => {
      result.current.jumpToBottom();
    });
    await Promise.resolve(); // flush adapter.scrollToBottom().then → vml.scrollToBottom

    // The programmatic scroll to the bottom fires a scroll event (scrollTop
    // moves DOWN).  This must NOT cancel followMode.
    container.scrollTop = 1600;
    act(() => {
      result.current.handleScroll();
    });
    expect(result.current.showScrollToBottom).toBe(false);

    // New stream content arrives → auto-scroll must still fire.
    vml.current.scrollToBottom.mockClear();
    fire({ type: "liveUpdate" });
    expect(vml.current.scrollToBottom).toHaveBeenCalledTimes(1);
  });

  it("contradiction resolver: followMode + needsLatestContent triggers jumpToBottom", async () => {
    // isAtTail=false keeps needsLatestContent=true; followMode defaults to
    // true → the resolver must resolve the contradiction by jumping.
    const scrollToBottom = vi.fn(async () => {});
    const vml = makeAutoFollowVml();
    const adapter = makeMockAdapter({ isAtTail: () => false, scrollToBottom });
    renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: vml,
        sessionKey: "agent:sess",
      }),
    );
    await act(async () => {});

    expect(scrollToBottom).toHaveBeenCalled();
  });

  // ── followMode re-arm (symmetric path) ──────────────────────────────

  // Container preset: simulates a session that's been scrolled up.  The
  // baseline "previous" scrollTop is what the controller's
  // prevScrollTopRef last saw — set it explicitly so the direction
  // detection is unambiguous.
  function makeScrolledContainer(scrollTop: number) {
    return {
      scrollTop,
      scrollHeight: 2000,
      clientHeight: 400, // distFromBottom = 2000 - scrollTop - 400
    } as unknown as HTMLDivElement;
  }

  it("user scroll-up then scroll-down into bottom zone re-arms followMode", () => {
    // Bug: a single scroll-up → scroll-down cycle used to leave followMode
    // stuck false, so new messages stopped auto-scrolling even though the
    // user was visually at the tail.  The symmetric re-arm path in
    // handleScroll fixes this.
    const adapter = makeMockAdapter({ isAtTail: () => false });
    const container = makeScrolledContainer(1600); // initially at bottom
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: container },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    // Mount: container.scrollTop=1600, followMode=true.
    // First handleScroll establishes the prevScrollTopRef baseline.
    act(() => {
      result.current.handleScroll();
    });
    expect(result.current.showScrollToBottom).toBe(false);

    // User scrolls up past the threshold (60 > 50): followMode cancels.
    container.scrollTop = 60;
    act(() => {
      result.current.handleScroll();
    });
    expect(result.current.showScrollToBottom).toBe(true);

    // User scrolls back down into the bottom zone (1590 ≤ 50 from
    // bottom): followMode re-arms automatically.
    container.scrollTop = 1590;
    act(() => {
      result.current.handleScroll();
    });
    expect(result.current.showScrollToBottom).toBe(false);
  });

  it("scroll-up exactly at threshold (50px) does NOT cancel followMode", () => {
    // Edge case: distFromBottom === EDGE_THRESHOLD_PX.  The check uses
    // `> threshold` (strict), so the boundary stays in follow mode.
    const adapter = makeMockAdapter({ isAtTail: () => false });
    const container = makeScrolledContainer(1550); // distFromBottom = 50
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: container },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    act(() => {
      result.current.handleScroll();
    });
    // First scroll event establishes the baseline; followMode stays true.
    expect(result.current.showScrollToBottom).toBe(false);

    // Scroll up 1px: distFromBottom becomes 51 > 50 → cancel.
    container.scrollTop = 1549;
    act(() => {
      result.current.handleScroll();
    });
    expect(result.current.showScrollToBottom).toBe(true);
  });

  it("scroll-down outside bottom zone does NOT re-arm followMode", () => {
    // Re-arm only fires when the user has scrolled DOWN *into* the
    // bottom zone.  Scrolling down while still far from the bottom
    // (e.g. user is reading mid-page and nudges the wheel) must not
    // hijack followMode back to true.
    const adapter = makeMockAdapter({ isAtTail: () => false });
    const container = makeScrolledContainer(1600); // start at bottom
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: container },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    // Baseline establishes the prevScrollTopRef.
    act(() => {
      result.current.handleScroll();
    });

    // Scroll up past the threshold so followMode is genuinely off.
    container.scrollTop = 60;
    act(() => {
      result.current.handleScroll();
    });
    expect(result.current.showScrollToBottom).toBe(true);

    // Scroll down to mid-page (distFromBottom = 2000-500-400 = 1100):
    // direction is DOWN, but the user is nowhere near the tail.
    container.scrollTop = 500;
    act(() => {
      result.current.handleScroll();
    });
    // followMode stays false because the re-arm path is gated on nearBottom.
    expect(result.current.showScrollToBottom).toBe(true);
  });

  it("liveUpdate keeps auto-scrolling after followMode is re-armed by returning to bottom", async () => {
    // End-to-end smoke: scroll-up → arrow visible → scroll-down into
    // bottom → re-arm → liveUpdate forces scrollToBottom again.
    const vml = makeAutoFollowVml();
    const { subscribe, fire } = makeDeferredSubscribe();
    const adapter = makeMockAdapter({ subscribe, isAtTail: () => false });
    const container = makeScrolledContainer(1600);
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: container },
        adapter,
        vmlRef: vml,
        sessionKey: "agent:sess",
      }),
    );

    // Baseline.
    act(() => {
      result.current.handleScroll();
    });
    // Scroll up past threshold → arrow appears, followMode off.
    container.scrollTop = 60;
    act(() => {
      result.current.handleScroll();
    });
    expect(result.current.showScrollToBottom).toBe(true);
    vml.current.scrollToBottom.mockClear();

    // Scroll back down into bottom zone → arrow hides, followMode on.
    container.scrollTop = 1590;
    act(() => {
      result.current.handleScroll();
    });
    expect(result.current.showScrollToBottom).toBe(false);

    // New stream content arrives → auto-scroll fires again.
    fire({ type: "liveUpdate" });
    expect(vml.current.scrollToBottom).toHaveBeenCalledTimes(1);
  });
});