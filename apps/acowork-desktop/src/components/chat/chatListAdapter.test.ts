/**
 * chatListAdapter — unit tests for the v2 blocksSelector.
 *
 * ADR-050 §7 C3 verification:
 *   "blocksSelector 在 atTail=true/false、liveBuffer empty/non-empty
 *    各种组合下行为正确"
 *
 * We can't easily mount the React hook without a full DOM
 * (useSyncExternalStore), so the tests target the inner snapshot
 * builder via a deliberately small surface: `buildSnapshot` is
 * private, but the same logic runs through `getSnapshot()` after the
 * store subscribes to the two upstream zustand stores.  We mount a
 * fresh adapter store, push state into chatStore + chatAdapterStore,
 * and read `getSnapshot()` directly.
 */
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { useChatStore } from "../../stores/chatStore";
import {
  useChatAdapterStore,
  ingestOptimisticUserMessage,
  ingestStreamDelta,
  ingestRecordComplete,
  releaseAdapterSession,
} from "./chatAdapterStore";
import {
  __adapterStoreFor,
  __releaseAdapterStore,
  __createAdapterStoreForTest,
} from "./chatListAdapter";
import type { ChatMessage } from "../../lib/types";

const AGENT = "com.test.Agent";
const SESSION = "sess-test";

// ── helpers ────────────────────────────────────────────────────────────────

function clearAllState() {
  // Drop every chatStore session
  useChatStore.setState((s) => ({ ...s, agentStates: {} }));
  // Drop every adapter session
  useChatAdapterStore.setState({ sessions: {} });
  // Drop every adapter store singleton
  __releaseAdapterStore(AGENT, SESSION);
  // Also drop any other agent states that might have leaked in.
  for (const a of Object.keys(useChatStore.getState().agentStates)) {
    useChatStore.getState().removeSessionState(a, SESSION);
  }
}

function seedHistoryInStore(messages: ChatMessage[], opts?: { offset?: number; limit?: number; total?: number }) {
  const offset = opts?.offset ?? 0;
  const limit = opts?.limit ?? messages.length;
  const total = opts?.total ?? messages.length;
  useChatStore.setState((s) => ({
    ...s,
    agentStates: {
      ...s.agentStates,
      [AGENT]: {
        ...(s.agentStates[AGENT] ?? {}),
        activeSessionId: SESSION,
        sessionStates: {
          ...(s.agentStates[AGENT]?.sessionStates ?? {}),
          [SESSION]: {
            ...(s.agentStates[AGENT]?.sessionStates?.[SESSION] ?? {}),
            messages,
            messageOffset: offset,
            messageLimit: limit,
            messageTotal: total,
          },
        },
      },
    },
  }));
}

function freshStore() {
  // Release first to ensure a clean store (no leftover subscriptions
  // from a previous test).
  __releaseAdapterStore(AGENT, SESSION);
  // Re-create by using the public hook would require React; instead
  // we expose a small constructor helper for tests below.
  return __adapterStoreFor(AGENT, SESSION) ?? __createAdapterStoreForTest(AGENT, SESSION);
}

function ts(base: number) { return base; }
function msg(id: string, timestamp: number, type: ChatMessage["type"] = "user", content: string = id): ChatMessage {
  return { id, type, content, timestamp };
}

// ── tests ─────────────────────────────────────────────────────────────────

describe("chatListAdapter v2: blocksSelector states", () => {
  beforeEach(() => {
    clearAllState();
  });

  afterEach(() => {
    clearAllState();
  });

  it("atTail=false with empty liveBuffer → blocks = foldMessages(history)", () => {
    seedHistoryInStore([msg("u1", ts(100)), msg("a1", ts(200))], { total: 100 });
    const store = freshStore();
    if (!store) throw new Error("store missing");
    const snap = store.getSnapshot();
    expect(snap.atTail).toBe(false);
    expect(snap.hasPendingFlush).toBe(false);
    expect(snap.blocks.length).toBeGreaterThan(0);
    // None of the blocks should be isLive since liveBuffer is empty.
    expect(snap.blocks.every((b) => b.isLive === false)).toBe(true);
  });

  it("atTail=true with empty liveBuffer → blocks = foldMessages(history), no isLive", () => {
    seedHistoryInStore([msg("u1", ts(100)), msg("a1", ts(200))], { total: 2 });
    const store = freshStore();
    if (!store) throw new Error("store missing");
    const snap = store.getSnapshot();
    expect(snap.atTail).toBe(true);
    expect(snap.hasPendingFlush).toBe(false);
    expect(snap.blocks.every((b) => b.isLive === false)).toBe(true);
  });

  it("atTail=false with non-empty liveBuffer → blocks still = history only (liveBuffer skipped)", () => {
    seedHistoryInStore([msg("u1", ts(100)), msg("a1", ts(200))], { total: 100 });
    ingestOptimisticUserMessage(AGENT, SESSION, [
      msg("pending-user", ts(50), "user"),
    ]);
    const store = freshStore();
    if (!store) throw new Error("store missing");
    const snap = store.getSnapshot();
    expect(snap.atTail).toBe(false);
    // pendingUserMessage is in liveBuffer but atTail=false → not folded in.
    expect(snap.hasPendingFlush).toBe(false);
    expect(snap.blocks.find((b) => b.items.some((i) => i.id === "pending-user"))).toBeUndefined();
  });

  it("atTail=true with non-empty liveBuffer → blocks include live entries, isLive=true", () => {
    seedHistoryInStore([msg("u1", ts(100)), msg("a1", ts(200))], { total: 2 });
    ingestOptimisticUserMessage(AGENT, SESSION, [
      msg("pending-user", ts(50), "user"),
    ]);
    const store = freshStore();
    if (!store) throw new Error("store missing");
    const snap = store.getSnapshot();
    expect(snap.atTail).toBe(true);
    expect(snap.hasPendingFlush).toBe(true);
    const pendingBlock = snap.blocks.find((b) =>
      b.items.some((i) => i.id === "pending-user"),
    );
    expect(pendingBlock).toBeDefined();
    expect(pendingBlock?.isLive).toBe(true);
  });

  it("dedup: liveBuffer entry with id already in history → no duplicate, history wins", () => {
    seedHistoryInStore([msg("u1", ts(100)), msg("a1", ts(200))], { total: 2 });
    // Optimistic user with the same id as the server's u1.
    ingestOptimisticUserMessage(AGENT, SESSION, [
      msg("u1", ts(99), "user", "stale-overlay"),
    ]);
    const store = freshStore();
    if (!store) throw new Error("store missing");
    const snap = store.getSnapshot();
    // The block containing u1 should have the server's content ("u1"),
    // not the optimistic overlay's ("stale-overlay").
    const u1Block = snap.blocks.find((b) => b.items.some((i) => i.id === "u1"));
    expect(u1Block).toBeDefined();
    const u1 = u1Block?.items.find((i) => i.id === "u1");
    expect(u1?.content).toBe("u1");
  });

  it("stream_delta (thought) when atTail → thinkingStream lands in blocks, isLive=true", () => {
    seedHistoryInStore([msg("u1", ts(100))], { total: 1 });
    ingestStreamDelta(AGENT, SESSION, [
      { role: "thought", message_id: "thought-1", line_no: 0, content: "reasoning..." },
    ]);
    const store = freshStore();
    if (!store) throw new Error("store missing");
    const snap = store.getSnapshot();
    expect(snap.atTail).toBe(true);
    expect(snap.hasPendingFlush).toBe(true);
    const thoughtBlock = snap.blocks.find((b) =>
      b.items.some((i) => i.id === "thought-1"),
    );
    expect(thoughtBlock).toBeDefined();
    expect(thoughtBlock?.isLive).toBe(true);
  });

  it("stream_delta (assistant) followed by record_complete → assistantStream → pendingRecordComplete", () => {
    seedHistoryInStore([msg("u1", ts(100))], { total: 1 });
    ingestStreamDelta(AGENT, SESSION, [
      { role: "assistant", message_id: "a-1", line_no: 0, content: "hi" },
    ]);
    // record_complete promotes the draft from assistantStream into
    // pendingRecordComplete[] — it should still be in the snapshot.
    ingestRecordComplete(AGENT, SESSION, {
      messageId: "a-1",
      role: "assistant",
    });
    const store = freshStore();
    if (!store) throw new Error("store missing");
    const snap = store.getSnapshot();
    const aBlock = snap.blocks.find((b) =>
      b.items.some((i) => i.id === "a-1"),
    );
    expect(aBlock).toBeDefined();
    expect(aBlock?.isLive).toBe(true);
  });

  it("pageLoaded event fires when chatStore's messageOffset/limit/total changes", () => {
    seedHistoryInStore([msg("u1", ts(100))], { offset: 0, limit: 1, total: 1 });
    const store = freshStore();
    if (!store) throw new Error("store missing");
    const events: Array<{ type: string; direction?: string }> = [];
    store.subscribe((ev) => events.push({ type: ev.type, direction: "direction" in ev ? ev.direction : undefined }) as any);
    // Simulate a loadNextPage by extending the window.
    seedHistoryInStore([msg("u1", ts(100)), msg("a1", ts(200))], { offset: 0, limit: 2, total: 2 });
    // Wait one microtask for the subscriber to fire.
    return new Promise<void>((resolve) => {
      queueMicrotask(() => {
        expect(events.some((e) => e.type === "pageLoaded")).toBe(true);
        resolve();
      });
    });
  });

  it("releaseAdapterStore teardown drops the singleton", () => {
    seedHistoryInStore([msg("u1", ts(100))], { total: 1 });
    const store = freshStore();
    if (!store) throw new Error("store missing");
    expect(__adapterStoreFor(AGENT, SESSION)).not.toBeNull();
    __releaseAdapterStore(AGENT, SESSION);
    expect(__adapterStoreFor(AGENT, SESSION)).toBeNull();
  });
});

// Ensure unused-import warnings stay clean.
void releaseAdapterSession;
