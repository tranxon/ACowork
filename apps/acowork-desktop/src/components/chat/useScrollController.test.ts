/**
 * useScrollController — unit tests for the C5 event-driven controller.
 *
 * ADR-050 §7 C5 verification:
 *   - tsc --noEmit: clean (smoke-checked outside)
 *   - Manual tests in §8 acceptance matrix
 *   - Unit tests pin the data-derived flag contract (the only piece
 *     that doesn't need a DOM):
 *       showScrollToBottom = !adapter.isAtTail() || adapter.hasPendingFlush()
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

function makeMockAdapter(overrides: Partial<ChatListAdapterV2> = {}): ChatListAdapterV2 {
  return {
    blocks: [],
    totalBlocks: 0,
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

  it("showScrollToBottom=true when at tail but pending flush (streaming)", () => {
    const adapter = makeMockAdapter({ isAtTail: () => true, hasPendingFlush: () => true });
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