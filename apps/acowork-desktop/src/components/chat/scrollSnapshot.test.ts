/**
 * Regression tests for session-switch scroll position restoration.
 *
 * Bug: When the user opens a historical session (lands at bottom), switches
 * to another session, then switches back, the scroll position lands in the
 * middle instead of at the bottom.  Root cause: pixel offset restoration
 * is unreliable when VML remounts because the virtualizer's per-instance
 * itemSizeCache is empty - items outside the previous viewport fall back
 * to SAFE_FALLBACK_HEIGHT (60px), making the initial totalSize much
 * smaller than the real content height.  The browser clamps scrollTop to
 * a mid-content position.
 *
 * Fix: When pinnedToBottom=true in the snapshot, return undefined (which
 * causes the scroll controller to call scrollToBottom() - data-driven and
 * self-correcting via reconcileScroll).  Only restore the pixel offset when
 * the user was browsing history (pinnedToBottom=false).
 */

import { describe, it, expect } from "vitest";
import { computeInitialScrollOffset } from "./ChatPanel";

describe("computeInitialScrollOffset", () => {
  // ── Scenario 1: First open (no snapshot) ──
  it("returns undefined when no snapshot exists (first open)", () => {
    expect(computeInitialScrollOffset(false, undefined)).toBeUndefined();
  });

  // ── Scenario 2: Streaming session ──
  it("returns undefined when sending=true (streaming)", () => {
    const snapshot = { scrollOffset: 5000, pinnedToBottom: true };
    expect(computeInitialScrollOffset(true, snapshot)).toBeUndefined();
  });

  it("returns undefined when sending=true even if pinnedToBottom=false", () => {
    const snapshot = { scrollOffset: 2000, pinnedToBottom: false };
    expect(computeInitialScrollOffset(true, snapshot)).toBeUndefined();
  });

  // ── Scenario 3: Return to a session where user was at the bottom ──
  // This is the core regression test for the reported bug.
  it("returns undefined when pinnedToBottom=true (user was at bottom)", () => {
    // The saved scrollOffset is the pixel position of the bottom, but
    // restoring it is unreliable because VML remounts with an empty
    // itemSizeCache.  Returning undefined causes scrollToBottom() to be
    // used instead, which is data-driven and self-corrects.
    const snapshot = { scrollOffset: 9200, pinnedToBottom: true };
    expect(computeInitialScrollOffset(false, snapshot)).toBeUndefined();
  });

  // ── Scenario 4: Return to a session where user was browsing history ──
  it("returns the saved scrollOffset when pinnedToBottom=false (user was browsing)", () => {
    const snapshot = { scrollOffset: 3500, pinnedToBottom: false };
    expect(computeInitialScrollOffset(false, snapshot)).toBe(3500);
  });

  // ── Scenario 5: Edge case - scrollOffset=0 with pinnedToBottom=false ──
  it("returns 0 when pinnedToBottom=false and scrollOffset=0 (user at top)", () => {
    const snapshot = { scrollOffset: 0, pinnedToBottom: false };
    expect(computeInitialScrollOffset(false, snapshot)).toBe(0);
  });

  // ── Scenario 6: Edge case - sending=true takes priority over everything ──
  it("sending=true takes priority over pinnedToBottom=false", () => {
    const snapshot = { scrollOffset: 1000, pinnedToBottom: false };
    expect(computeInitialScrollOffset(true, snapshot)).toBeUndefined();
  });

  // ── Scenario 7: Real-world reproduction ──
  // User opens historical session A (no snapshot) -> scrollToBottom.
  // User switches to B -> snapshot for A saved with pinnedToBottom=true.
  // User switches back to A -> should get undefined (scrollToBottom).
  it("reproduces: switch away from bottom-pinned session and back", () => {
    // Step 1: First open A - no snapshot
    expect(computeInitialScrollOffset(false, undefined)).toBeUndefined();

    // Step 2: Switch away - snapshot saved with pinnedToBottom=true
    const snapshotAfterLeavingA = { scrollOffset: 9200, pinnedToBottom: true };

    // Step 3: Switch back to A - should NOT restore pixel offset
    expect(computeInitialScrollOffset(false, snapshotAfterLeavingA)).toBeUndefined();
  });

  // ── Scenario 8: Real-world reproduction - user scrolled up then switched ──
  it("reproduces: switch away from browsing session and back", () => {
    // User scrolled up in session A, then switched to B
    const snapshotAfterLeavingA = { scrollOffset: 2000, pinnedToBottom: false };

    // Switch back to A - should restore pixel offset
    expect(computeInitialScrollOffset(false, snapshotAfterLeavingA)).toBe(2000);
  });
});
