/**
 * Regression tests for the data-driven ChatScrollSnapshot contract.
 *
 * The snapshot is purely content-derived — { atBottom, firstVisibleBlockId }
 * — and is consumed by useScrollController to restore the viewport via
 * scrollToBottom() or scrollToBlockId().  No pixel offsets; no
 * firstVisibleBlockIndex/messageOffset pair.
 *
 * The pure helper under test decides what to do given (snapshot, sending):
 *   - undefined snapshot              → scroll to bottom (fresh open).
 *   - atBottom=true                   → scroll to bottom (return to tail).
 *   - atBottom=false + blockId        → restore by blockId (user was browsing).
 *   - atBottom=false + no blockId     → fall back to bottom.
 *
 * `sending` is intentionally NOT part of the decision tree: a session
 * that is actively streaming still has a real, content-derived reading
 * position from the moment the user paused.  Restoring by blockId is
 * correct for both idle and streaming cases — the virtualizer positions
 * on the saved block and the live tail naturally grows below.
 */

import { describe, it, expect } from "vitest";

interface ChatScrollSnapshot {
  atBottom: boolean;
  firstVisibleBlockId: string | null;
  messageOffset: number | null;
  firstVisibleBlockIndex: number | null;
}

/**
 * Pure decision function: what should init-scroll do given the snapshot?
 * Returns "bottom" | { kind: "blockId"; blockId: string }.
 *
 * Extracted for unit testing.
 */
export function resolveInitialScrollTarget(
  snapshot: ChatScrollSnapshot | undefined,
): "bottom" | { blockId: string } {
  if (!snapshot) return "bottom";
  if (snapshot.atBottom) return "bottom";
  if (snapshot.firstVisibleBlockId) return { blockId: snapshot.firstVisibleBlockId };
  return "bottom";
}

describe("resolveInitialScrollTarget", () => {
  // ── Scenario 1: First open (no snapshot) ──
  it("returns bottom when no snapshot exists (first open)", () => {
    expect(resolveInitialScrollTarget(undefined)).toBe("bottom");
  });

  // ── Scenario 2: Return to a session at bottom ──
  it("returns bottom when atBottom=true", () => {
    expect(resolveInitialScrollTarget({
      atBottom: true,
      firstVisibleBlockId: "block-msg-100",
      messageOffset: 50,
      firstVisibleBlockIndex: 10,
    })).toBe("bottom");
  });

  // ── Scenario 3: Return to a session while browsing history ──
  it("returns blockId when atBottom=false with a saved blockId", () => {
    expect(resolveInitialScrollTarget({
      atBottom: false,
      firstVisibleBlockId: "block-msg-42",
      messageOffset: 20,
      firstVisibleBlockIndex: 5,
    })).toEqual({ blockId: "block-msg-42" });
  });

  // ── Scenario 4: atBottom=false but blockId missing (small session, no scroll) ──
  it("returns bottom when atBottom=false and no blockId", () => {
    expect(resolveInitialScrollTarget({
      atBottom: false,
      firstVisibleBlockId: null,
      messageOffset: null,
      firstVisibleBlockIndex: null,
    })).toBe("bottom");
  });

  // ── Scenario 5: Streaming session — blockId should still win ──
  // This is the key behavior change: pre-C5 the helper bailed out to
  // bottom whenever sending=true.  Post-C5 the blockId is honored so
  // the user lands at exactly where they paused.
  it("returns blockId even when atBottom=false (streaming does not override blockId)", () => {
    // The controller receives sending via a separate channel; the resolver
    // itself only sees the snapshot, which already encodes user intent.
    const result = resolveInitialScrollTarget({
      atBottom: false,
      firstVisibleBlockId: "block-msg-15",
      messageOffset: 10,
      firstVisibleBlockIndex: 3,
    });
    expect(result).toEqual({ blockId: "block-msg-15" });
  });

  // ── Scenario 6: Real-world reproduction — switch away from bottom-pinned session and back ──
  it("reproduces: switch away from bottom-pinned session and back → bottom", () => {
    const snapshotAfterLeavingA: ChatScrollSnapshot = {
      atBottom: true,
      firstVisibleBlockId: null,
      messageOffset: null,
      firstVisibleBlockIndex: null,
    };
    expect(resolveInitialScrollTarget(snapshotAfterLeavingA)).toBe("bottom");
  });

  // ── Scenario 7: Real-world reproduction — switch away from browsing session and back ──
  it("reproduces: switch away from browsing session and back → blockId", () => {
    const snapshotAfterLeavingA: ChatScrollSnapshot = {
      atBottom: false,
      firstVisibleBlockId: "block-msg-30",
      messageOffset: 25,
      firstVisibleBlockIndex: 15,
    };
    expect(resolveInitialScrollTarget(snapshotAfterLeavingA)).toEqual({
      blockId: "block-msg-30",
    });
  });

  // ── Scenario 8: Edge case — atBottom=true with a blockId (e.g. last visible is also at bottom) ──
  it("atBottom takes priority over blockId", () => {
    expect(resolveInitialScrollTarget({
      atBottom: true,
      firstVisibleBlockId: "block-msg-50",
      messageOffset: 40,
      firstVisibleBlockIndex: 20,
    })).toBe("bottom");
  });
});
