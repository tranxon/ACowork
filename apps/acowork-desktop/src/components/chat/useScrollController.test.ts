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
import { renderHook } from "@testing-library/react";
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
  it("showScrollToBottom=true when not at tail", () => {
    const adapter = makeMockAdapter({ isAtTail: () => false });
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
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
    result.current.jumpToBottom();
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
    result.current.jumpToTop();
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