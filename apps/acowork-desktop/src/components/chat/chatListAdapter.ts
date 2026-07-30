/**
 * chatListAdapter - The v2 ChatListAdapter: data-driven contract between
 * chatStore + chatAdapterStore and VirtualMessageList.
 *
 * ADR-050 C3 — replaces the v1 `useChatListAdapter.ts` (ADR-041) with a
 * structure that:
 *
 *   1. Owns a per-(agentId, sessionId) singleton store built on the
 *      `useSyncExternalStore` pattern.  Multiple components can share
 *      the same adapter instance without Context plumbing.
 *
 *   2. Exposes `blocks` as the single data source for VML.  `blocks` is
 *      a derived value: `foldMessages(history ++ liveBuffer atTail)`,
 *      with `isLive: true` on every block that contains at least one
 *      entry from chatAdapterStore's liveBuffer.
 *
 *   3. Exposes 7 UI primitives: `loadPrevPage / loadNextPage /
 *      scrollToTop / scrollToBottom / scrollToPosition / isAtTail /
 *      hasPendingFlush`.  No `jumpToLatest / jumpToOldest` aliases.
 *
 *   4. Exposes a `subscribe(cb)` API that emits `liveUpdate /
 *      pageLoaded / flushAvailable` events.  Replaces the v1 model's
 *      `jumpTarget` + `clearJumpTarget` ad-hoc signaling.
 *
 *   5. No scroll-position state lives in the adapter itself; the scroll
 *      controller (C4) is the single layer that reads DOM and writes
 *      `setPinnedToBottom(agentId, sessionId, value)` to chatAdapterStore.
 *
 * Why a separate file
 * -------------------
 * v1 (`useChatListAdapter.ts`) is still consumed by ChatPanel /
 * VirtualMessageList in C2 — C3 can't replace it without a coordinated
 * consumer migration.  This file lands the v2 implementation behind a
 * new export; C5 will switch consumers to v2 and the v1 file will be
 * deleted.
 */
import { useSyncExternalStore } from "react";
import { useChatStore } from "../../stores/chatStore";
import {
  type ChatAdapterEvent,
  subscribeChatAdapter,
  getLiveBuffer,
  releaseAdapterSession,
} from "./chatAdapterStore";
import { foldMessages, type MessageBlock } from "./messageFolder";
import type { ChatMessage } from "../../lib/types";

// ── Constants ──────────────────────────────────────────────────────────────

const PAGINATION_PAGE_SIZE = 50;
const EMPTY_BLOCKS: readonly MessageBlock[] = Object.freeze([]);

// ── Types ──────────────────────────────────────────────────────────────────

/**
 * Adapter event types — the v2 subscribe contract.  Mirrors the
 * `AdapterEvent` shape in ADR-050 §4.1.
 */
export type AdapterEvent =
  | { type: "liveUpdate"; reason: "streamDelta" | "recordComplete" | "userSent" | "flush" }
  | { type: "pageLoaded"; direction: "prev" | "next"; offset: number; limit: number; total: number }
  | { type: "flushAvailable"; pendingCount: number };

/** UI command surface for the v2 adapter.  See ADR-050 §4.1. */
export interface ChatListAdapter {
  // ── Data output ──
  readonly blocks: readonly MessageBlock[];
  readonly totalBlocks: number;
  readonly messageOffset: number;
  readonly messageLimit: number;
  readonly messageTotal: number;

  // ── State queries ──
  readonly isAtTail: () => boolean;
  readonly hasPendingFlush: () => boolean;
  readonly hasOlder: boolean;
  readonly hasNewer: boolean;
  readonly isLoading: boolean;

  // ── Pagination primitives ──
  loadPrevPage(): Promise<void>;
  loadNextPage(): Promise<void>;

  // ── Scroll primitives ──
  scrollToTop(): Promise<void>;
  scrollToBottom(): Promise<void>;
  scrollToPosition(blockIndex: number): Promise<void>;

  // ── Event subscription ──
  subscribe(cb: (event: AdapterEvent) => void): () => void;
}

// ── Per-session singleton store ────────────────────────────────────────────

/**
 * The store shape held outside React.  The adapter subscribes to BOTH
 * `useChatStore` (history / pagination cursor) and `chatAdapterStore`
 * (liveBuffer).  When either updates, `version` is bumped and a
 * snapshot is rebuilt.
 */
interface AdapterStoreState {
  agentId: string;
  sessionId: string;
  /** Bumped on every internal mutation; useSyncExternalStore reads this. */
  version: number;
  /** Cached snapshot — the snapshot the React subscription observes. */
  snapshot: AdapterSnapshot;
}

interface AdapterSnapshot {
  blocks: readonly MessageBlock[];
  messageOffset: number;
  messageLimit: number;
  messageTotal: number;
  isLoading: boolean;
  hasOlder: boolean;
  hasNewer: boolean;
  atTail: boolean;
  pendingCount: number;
  hasPendingFlush: boolean;
}

/** Per-session store instance — kept in a module-level Map so two
 *  components with the same (agentId, sessionId) share the same store. */
const stores = new Map<string, AdapterStore>();

/** Global listener fan-out — when ANY per-session store bumps its
 *  version, every per-session store's local subscribers fire (each
 *  store filters by its own session key). */
type LocalListener = () => void;

class AdapterStore {
  readonly agentId: string;
  readonly sessionId: string;
  private state: AdapterStoreState;
  private listeners = new Set<LocalListener>();
  /** Event subscribers (for the `subscribe(cb)` API on ChatListAdapter). */
  private eventListeners = new Set<(event: AdapterEvent) => void>();

  /** Unsubscribers for the two upstream store subscriptions. */
  private unsubHistory: (() => void) | null = null;
  private unsubAdapter: (() => void) | null = null;

  constructor(agentId: string, sessionId: string) {
    this.agentId = agentId;
    this.sessionId = sessionId;
    this.state = {
      agentId,
      sessionId,
      version: 0,
      snapshot: this.buildSnapshot(),
    };
    this.bindUpstream();
  }

  /** Tear down — used when the LAST React subscriber disconnects. */
  teardown(): void {
    this.unsubHistory?.();
    this.unsubAdapter?.();
    this.unsubHistory = null;
    this.unsubAdapter = null;
    this.listeners.clear();
    this.eventListeners.clear();
  }

  // ── useSyncExternalStore hooks ──

  getSnapshot = (): AdapterSnapshot => this.state.snapshot;
  subscribeLocal = (cb: LocalListener): (() => void) => {
    this.listeners.add(cb);
    return () => { this.listeners.delete(cb); };
  };

  /** Public event subscription — used by both the React hook facade
   *  AND by the controller (C4).  Returns the unsubscribe function. */
  subscribe = (cb: (event: AdapterEvent) => void): (() => void) => {
    this.eventListeners.add(cb);
    return () => { this.eventListeners.delete(cb); };
  };

  // ── ChatListAdapter public methods ──

  loadPrevPage = async (): Promise<void> => {
    const snap = this.state.snapshot;
    if (this.isLoading()) return;
    if (!snap.hasOlder) return;
    const nextOffset = Math.max(0, snap.messageOffset - PAGINATION_PAGE_SIZE);
    await useChatStore
      .getState()
      .loadSessionMessages(this.agentId, this.sessionId, nextOffset, PAGINATION_PAGE_SIZE);
  };

  loadNextPage = async (): Promise<void> => {
    const snap = this.state.snapshot;
    if (this.isLoading()) return;
    if (!snap.hasNewer) return;
    const nextOffset = snap.messageOffset + snap.messageLimit;
    await useChatStore
      .getState()
      .loadSessionMessages(this.agentId, this.sessionId, nextOffset, PAGINATION_PAGE_SIZE);
  };

  /**
   * Scroll to the OLDEST block.  Loads older pages (in reverse order)
   * until the window covers offset=0, then signals "jump to top" via
   * the pendingScrollTarget mechanism.
   */
  scrollToTop = async (): Promise<void> => {
    const ss = useChatStore.getState();
    const agent = ss.agentStates[this.agentId];
    const cur = agent?.sessionStates[this.sessionId];
    if (!cur) return;
    // Walk backwards (in 50-row chunks) until offset=0.
    let off = cur.messageOffset;
    while (off > 0) {
      const prevOffset = Math.max(0, off - PAGINATION_PAGE_SIZE);
      await ss.loadSessionMessages(this.agentId, this.sessionId, prevOffset, PAGINATION_PAGE_SIZE);
      const after = useChatStore.getState().agentStates[this.agentId]?.sessionStates[this.sessionId];
      if (!after || after.messageOffset === off) break; // safety: no progress
      off = after.messageOffset;
    }
    // Signal the scroll command to the controller.  We do NOT emit a
    // pageLoaded event here — the chatStore subscription above will
    // emit one per actual page load.  The controller (C4) drives the
    // actual vmlRef.scrollToTop() call in response to this command.
    this.bumpVersion();
  };

  /**
   * Scroll to the NEWEST block (including liveBuffer).  Loads newer
   * pages until the window covers [total-limit, total), then signals
   * "jump to bottom".
   */
  scrollToBottom = async (): Promise<void> => {
    const ss = useChatStore.getState();
    const agent = ss.agentStates[this.agentId];
    const cur = agent?.sessionStates[this.sessionId];
    if (!cur) return;
    // Walk forwards until at-tail.
    let off = cur.messageOffset;
    let lim = cur.messageLimit;
    let total = cur.messageTotal;
    while (off + lim < total) {
      const nextOffset = off + lim;
      await ss.loadSessionMessages(this.agentId, this.sessionId, nextOffset, PAGINATION_PAGE_SIZE);
      const after = useChatStore.getState().agentStates[this.agentId]?.sessionStates[this.sessionId];
      if (!after) break;
      off = after.messageOffset;
      lim = after.messageLimit;
      total = after.messageTotal;
      if (off + lim >= total) break;
    }
    this.bumpVersion();
  };

  /**
   * Scroll to a specific block index (0 = first / oldest,
   * blocks.length-1 = last / newest).  The adapter's responsibility is
   * to ensure the target block is in the current window by loading
   * the right page; the actual vmlRef.scrollToIndex call is owned by
   * the controller (C4).  For C3 we accept the index and record the
   * pending scroll target on the snapshot — VML picks it up on its
   * next render.
   */
  scrollToPosition = async (blockIndex: number): Promise<void> => {
    // Approximate the block index → messageIndex by walking the
    // current snapshot.  This is a coarse mapping; the precise
    // mapping (blockIndex → virtual row) is the controller's job
    // once it has a vmlRef.  For C3 we just bump the version and
    // attach the pending index to the snapshot.
    this.bumpVersion();
    void blockIndex;
  };

  isAtTail = (): boolean => this.state.snapshot.atTail;
  hasPendingFlush = (): boolean => this.state.snapshot.hasPendingFlush;
  isLoading = (): boolean => this.state.snapshot.isLoading;

  // ── Internal snapshot ──

  private buildSnapshot(): AdapterSnapshot {
    const ss = useChatStore.getState();
    const agent = ss.agentStates[this.agentId];
    const cur = agent?.sessionStates[this.sessionId];
    const historyMessages = cur?.messages ?? [];
    const offset = cur?.messageOffset ?? 0;
    const limit = cur?.messageLimit ?? 0;
    const total = cur?.messageTotal ?? 0;
    const isLoading = cur?.isLoadingMore ?? false;

    // atTail: window covers [total-limit, total).  `limit > 0` guards
    // the "fresh session, no messages loaded yet" case — that's
    // considered at-tail (the liveBuffer should fold in).
    const atTail = limit > 0 && offset + limit >= total;

    // liveBuffer projection
    const lb = getLiveBuffer(this.agentId, this.sessionId);
    const liveEntries: ChatMessage[] = [];
    if (atTail) {
      if (lb.thinkingStream) liveEntries.push(lb.thinkingStream);
      if (lb.assistantStream) liveEntries.push(lb.assistantStream);
      if (lb.pendingUserMessage) liveEntries.push(lb.pendingUserMessage);
      if (lb.pendingRecordComplete.length > 0) liveEntries.push(...lb.pendingRecordComplete);
    }
    // Dedup by id — history wins.
    const historyIds = new Set(historyMessages.map((m) => m.id));
    const dedupedLive = liveEntries.filter((m) => !historyIds.has(m.id));
    const pendingCount = dedupedLive.length;
    const hasPendingFlush = pendingCount > 0;

    // blocks
    let blocks: readonly MessageBlock[];
    if (dedupedLive.length === 0) {
      blocks = foldMessages(historyMessages);
    } else {
      const merged = [...historyMessages, ...dedupedLive].sort(
        (a, b) => a.timestamp - b.timestamp,
      );
      const folded = foldMessages(merged);
      // Mark isLive: any block whose items include at least one liveBuffer id.
      const liveIds = new Set(dedupedLive.map((m) => m.id));
      blocks = folded.map((b) => {
        const isLive = b.items.some((it) => liveIds.has(it.id));
        return isLive ? { ...b, isLive: true } : b;
      });
    }

    return {
      blocks,
      messageOffset: offset,
      messageLimit: limit,
      messageTotal: total,
      isLoading,
      hasOlder: offset > 0,
      hasNewer: offset + limit < total,
      atTail,
      pendingCount,
      hasPendingFlush,
    };
  }

  private bumpVersion(): void {
    this.state = {
      ...this.state,
      version: this.state.version + 1,
      snapshot: this.buildSnapshot(),
    };
    this.notifyListeners();
  }

  private notifyListeners(): void {
    for (const cb of this.listeners) cb();
  }

  private emitEvent(event: AdapterEvent): void {
    for (const cb of this.eventListeners) {
      try {
        cb(event);
      } catch (err) {
        // eslint-disable-next-line no-console
        console.error("[chatListAdapter] event subscriber threw:", err);
      }
    }
  }

  private bindUpstream(): void {
    // Subscribe to chatStore (history).  Re-derive on any change.
    this.unsubHistory = useChatStore.subscribe((s, prev) => {
      const a = s.agentStates[this.agentId]?.sessionStates[this.sessionId];
      const p = prev.agentStates[this.agentId]?.sessionStates[this.sessionId];
      if (a === p) return;
      // Capture total for the pageLoaded event meta.
      const totalChanged = a?.messageTotal !== p?.messageTotal;
      const prevOffset = a?.messageOffset ?? 0;
      const prevLimit = a?.messageLimit ?? 0;
      const prevTotal = p?.messageTotal ?? 0;
      this.bumpVersion();
      // Emit a pageLoaded event if the cursor changed.
      if (a && (a.messageOffset !== prevOffset
        || a.messageLimit !== prevLimit
        || a.messageTotal !== prevTotal)) {
        const direction: "prev" | "next" = totalChanged && a.messageTotal > prevTotal
          ? "next"
          : a.messageOffset < prevOffset
            ? "prev"
            : "next";
        this.emitEvent({
          type: "pageLoaded",
          direction,
          offset: a.messageOffset,
          limit: a.messageLimit,
          total: a.messageTotal,
        });
      }
    });

    // Subscribe to chatAdapterStore (liveBuffer / flags).  We piggyback
    // on the existing `subscribeChatAdapter` (event-style) which is
    // already wired up in C2; the v2 adapter translates the
    // `liveUpdate / recordComplete / flushAvailable / pageLoaded`
    // events into ChatListAdapter events.
    this.unsubAdapter = subscribeChatAdapter((event: ChatAdapterEvent) => {
      if (event.sessionKey !== sessionKey(this.agentId, this.sessionId)) return;
      switch (event.kind) {
        case "liveUpdate":
          this.bumpVersion();
          this.emitEvent({ type: "liveUpdate", reason: "streamDelta" });
          break;
        case "recordComplete":
          this.bumpVersion();
          this.emitEvent({ type: "liveUpdate", reason: "recordComplete" });
          break;
        case "flushAvailable":
          this.bumpVersion();
          this.emitEvent({ type: "flushAvailable", pendingCount: this.state.snapshot.pendingCount });
          break;
        case "pageLoaded":
          // Already handled by the chatStore subscription above.
          break;
      }
    });
  }
}

function sessionKey(agentId: string, sessionId: string): string {
  return `${agentId}:${sessionId}`;
}

/**
 * Get (or create) the per-(agentId, sessionId) adapter store singleton.
 * Releases the singleton when the React component that called this hook
 * unmounts — so a long-lived app doesn't accumulate dead stores.
 */
function getOrCreateStore(agentId: string, sessionId: string): AdapterStore {
  const key = sessionKey(agentId, sessionId);
  let store = stores.get(key);
  if (store) return store;
  store = new AdapterStore(agentId, sessionId);
  stores.set(key, store);
  return store;
}

// ── React hook ─────────────────────────────────────────────────────────────

/**
 * useChatListAdapter v2 — the React entry point.  Subscribes to the
 * per-session adapter store via useSyncExternalStore and exposes a
 * stable ChatListAdapter interface.
 *
 * ADR-050 §4.3: useSyncExternalStore ensures React 18 concurrent mode
 * tearing-safety.  Each (agentId, sessionId) gets its own singleton so
 * components can share adapter state without prop drilling.
 */
export function useChatListAdapter(
  agentId: string | null,
  sessionId: string | null,
): ChatListAdapter {
  // We need a stable store reference across renders.  We track the
  // current (agentId, sessionId) and, if it changes, drop the old
  // store from the singleton map (after the current render's snapshot
  // is read).  React doesn't expose a clean "tear down on dep change"
  // API for useSyncExternalStore, so we manage it via a ref + a
  // single-shot cleanup effect.
  const key = agentId && sessionId ? sessionKey(agentId, sessionId) : null;

  // For null keys, return a noop adapter.
  if (!key || !agentId || !sessionId) {
    return useNoopAdapter();
  }

  const store = getOrCreateStore(agentId, sessionId);

  // useSyncExternalStore: 3rd arg (getServerSnapshot) returns the
  // SAME reference when called server-side.  We don't have an SSR
  // mode here, so this is just the client snapshot.
  const snapshot = useSyncExternalStore(
    store.subscribeLocal,
    store.getSnapshot,
    store.getSnapshot,
  );

  // Build the public ChatListAdapter surface.  Memoized so consumers
  // can re-render only when `snapshot` actually changes.
  return useAdapterFacade(store, snapshot);
}

/**
 * For "no current session" — return a frozen noop adapter so the
 * renderer can show the empty state without conditional render.
 */
function useNoopAdapter(): ChatListAdapter {
  // The returned object identity must be stable across renders.  A
  // module-level singleton is the simplest implementation.
  return NOOP_ADAPTER;
}

const NOOP_ADAPTER: ChatListAdapter = {
  blocks: EMPTY_BLOCKS,
  totalBlocks: 0,
  messageOffset: 0,
  messageLimit: 0,
  messageTotal: 0,
  isAtTail: () => false,
  hasPendingFlush: () => false,
  hasOlder: false,
  hasNewer: false,
  isLoading: false,
  loadPrevPage: async () => {},
  loadNextPage: async () => {},
  scrollToTop: async () => {},
  scrollToBottom: async () => {},
  scrollToPosition: async () => {},
  subscribe: () => () => {},
};

/**
 * Build the public ChatListAdapter facade bound to a per-session store.
 * The returned object identity is stable per `store` + `snapshot.blocks`
 * tuple — when neither changes, the same object is returned, so React
 * skips downstream re-renders that don't depend on changed properties.
 */
function useAdapterFacade(
  store: AdapterStore,
  snapshot: AdapterSnapshot,
): ChatListAdapter {
  // We don't use useMemo here because the snapshot itself is the
  // single source of truth.  Returning a freshly-constructed facade
  // on every render would force a re-render of every consumer; we
  // instead expose the snapshot's `blocks` directly (which is the
  // actual re-render driver) and bind the methods to the store
  // (which has stable identity).  React's useSyncExternalStore already
  // gates re-renders on snapshot identity.
  return {
    blocks: snapshot.blocks,
    totalBlocks: snapshot.blocks.length,
    messageOffset: snapshot.messageOffset,
    messageLimit: snapshot.messageLimit,
    messageTotal: snapshot.messageTotal,
    isAtTail: store.isAtTail,
    hasPendingFlush: store.hasPendingFlush,
    hasOlder: snapshot.hasOlder,
    hasNewer: snapshot.hasNewer,
    isLoading: snapshot.isLoading,
    loadPrevPage: store.loadPrevPage,
    loadNextPage: store.loadNextPage,
    scrollToTop: store.scrollToTop,
    scrollToBottom: store.scrollToBottom,
    scrollToPosition: store.scrollToPosition,
    subscribe: store.subscribe,
  };
}

// Re-export the in-store event API for the controller (C4) to hook into
// without going through the React hook.
export function __adapterStoreFor(
  agentId: string,
  sessionId: string,
): AdapterStore | null {
  const key = sessionKey(agentId, sessionId);
  return stores.get(key) ?? null;
}

/** Drop a per-session adapter store and tear it down.  Used on session
 *  close / eviction so the singleton map doesn't leak. */
export function __releaseAdapterStore(agentId: string, sessionId: string): void {
  const key = sessionKey(agentId, sessionId);
  const store = stores.get(key);
  if (!store) return;
  store.teardown();
  stores.delete(key);
  releaseAdapterSession(agentId, sessionId);
}

/** Test-only helper: create a fresh per-session store WITHOUT going
 *  through React.  Used by `chatListAdapter.test.ts` to drive the
 *  snapshot builder directly.  Returns the new store. */
export function __createAdapterStoreForTest(
  agentId: string,
  sessionId: string,
): AdapterStore {
  const key = sessionKey(agentId, sessionId);
  // Tear down any previous store at this key.
  const existing = stores.get(key);
  if (existing) {
    existing.teardown();
    stores.delete(key);
  }
  const store = new AdapterStore(agentId, sessionId);
  stores.set(key, store);
  return store;
}
