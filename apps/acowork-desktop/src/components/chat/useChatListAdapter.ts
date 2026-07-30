/**
 * useChatListAdapter - The single bridge between chatStore (data layer) and
 * VirtualMessageList (rendering layer).
 *
 * Design:
 *  - Folds raw ChatMessage[] into MessageBlock[] with stable, content-derived
 *    blockId (survives prepend/append).
 *  - loadPrevPage/loadNextPage read state from useChatStore.getState() at call
 *    time, NOT from closure variables. This eliminates stale closures
 *    entirely - the function always sees the latest store state.
 *  - Pagination is triggered by a timer in ChatPanel (150ms interval),
 *    not by onScroll events or React effects. The timer reads scrollTop
 *    from the DOM and isLoading/hasOlder/hasNewer from the store.
 *  - Scroll position after prepend is maintained by VML's scrollHeight
 *    delta effect (classic infinite-scroll technique).
 *
 * ADR-050: pagination uses **forward (oldest-end) offset semantics**.
 *  - `messageOffset === 0`      → window anchored at the OLDEST entry
 *  - `messageOffset + limit >= total` → window touches the NEWEST entry
 *  - `hasOlder` = `messageOffset > 0`                      (older entries available)
 *  - `hasNewer` = `messageOffset + limit < messageTotal`    (newer entries available)
 *  - `loadPrevPage` → `nextOffset = max(0, messageOffset - limit)` (older direction)
 *  - `loadNextPage` → `nextOffset = messageOffset + limit`        (newer direction)
 */

import { useCallback, useMemo, useRef, useState } from "react";
import { useChatStore } from "../../stores/chatStore";
import { foldMessages, type MessageBlock } from "./messageFolder";
import type { ChatMessage } from "../../lib/types";

// ── Types ─────────────────────────────────────────────────────────────────

export interface ChatListAdapter {
  readonly blocks: MessageBlock[];
  readonly hasOlder: boolean;
  readonly hasNewer: boolean;
  readonly isLoading: boolean;
  readonly messageOffset: number;
  readonly messageLimit: number;
  readonly messageTotal: number;
  loadPrevPage: () => Promise<void>;
  loadNextPage: () => Promise<void>;
  jumpToLatest: () => Promise<void>;
  jumpToOldest: () => Promise<void>;
  readonly jumpTarget: "top" | "bottom" | null;
  clearJumpTarget: () => void;
}

// ── Constants ─────────────────────────────────────────────────────────────

const PAGINATION_PAGE_SIZE = 50;
const EMPTY_MESSAGES: ChatMessage[] = [];

// ── Helpers ───────────────────────────────────────────────────────────────

function setSessionLoadingMore(agentId: string, sessionId: string, value: boolean) {
  useChatStore.setState((state) => {
    const agent = state.agentStates[agentId];
    if (!agent || !agent.sessionStates[sessionId]) return state;
    return {
      ...state,
      agentStates: {
        ...state.agentStates,
        [agentId]: {
          ...agent,
          sessionStates: {
            ...agent.sessionStates,
            [sessionId]: { ...agent.sessionStates[sessionId], isLoadingMore: value },
          },
        },
      },
    };
  });
}

// ── Hook ──────────────────────────────────────────────────────────────────

export function useChatListAdapter(
  agentId: string | null,
  sessionId: string | null,
): ChatListAdapter {
  // ── Subscribe to store for React rendering ──
  // These selectors drive React re-renders (blocks, UI flags).
  // loadPrevPage/loadNextPage do NOT use these values - they read from
  // useChatStore.getState() at call time for the latest state.

  const messages = useChatStore((s) => {
    if (!agentId) return EMPTY_MESSAGES;
    const agent = s.agentStates[agentId];
    if (!agent) return EMPTY_MESSAGES;
    if (!sessionId) return EMPTY_MESSAGES;
    return agent.sessionStates[sessionId]?.messages ?? EMPTY_MESSAGES;
  });

  // Optimistic user-message overlay. The chatStore merges this into
  // `messages[]` whenever the HTTP window lands (same id → optimistic
  // copy is dropped, server copy kept; different id → optimistic copy
  // stays and `mergeMessageWindow` sorts it into position by timestamp).
  //
  // We re-derive a display array by appending, then sorting by timestamp,
  // so the block fold sees the exact same ordered stream the merge
  // function would have produced. This keeps the block layer
  // (`foldMessages`) ignorant of overlay mechanics while guaranteeing
  // the user sees their optimistic bubble in the right place.
  const optimisticEntries = useChatStore((s) => {
    if (!agentId) return EMPTY_MESSAGES;
    const agent = s.agentStates[agentId];
    if (!agent || !sessionId) return EMPTY_MESSAGES;
    return agent.sessionStates[sessionId]?.optimisticEntries ?? EMPTY_MESSAGES;
  });

  const displayMessages = useMemo<ChatMessage[]>(() => {
    if (optimisticEntries.length === 0) return messages;
    // Dedupe by id (defensive — store already dedupes, but a stale
    // subscription could momentarily return both copies if the merge
    // landed between two selector reads).
    const seen = new Set(messages.map((m) => m.id));
    const pending = optimisticEntries.filter((m) => !seen.has(m.id));
    if (pending.length === 0) return messages;
    return [...messages, ...pending].sort((a, b) => a.timestamp - b.timestamp);
  }, [messages, optimisticEntries]);

  const messageOffset = useChatStore((s) => {
    if (!agentId) return 0;
    const agent = s.agentStates[agentId];
    if (!agent || !sessionId) return 0;
    return agent.sessionStates[sessionId]?.messageOffset ?? 0;
  });

  const messageLimit = useChatStore((s) => {
    if (!agentId) return 0;
    const agent = s.agentStates[agentId];
    if (!agent || !sessionId) return 0;
    return agent.sessionStates[sessionId]?.messageLimit ?? 0;
  });

  const messageTotal = useChatStore((s) => {
    if (!agentId) return 0;
    const agent = s.agentStates[agentId];
    if (!agent || !sessionId) return 0;
    return agent.sessionStates[sessionId]?.messageTotal ?? 0;
  });

  const isLoadingMore = useChatStore((s) => {
    if (!agentId) return false;
    const agent = s.agentStates[agentId];
    if (!agent || !sessionId) return false;
    return agent.sessionStates[sessionId]?.isLoadingMore ?? false;
  });

  const hasOlder = messageOffset > 0;                                  // older entries available (scroll-up)
  const hasNewer = messageOffset + messageLimit < messageTotal;       // newer entries available (scroll-down)

  const blocks = useMemo<MessageBlock[]>(
    () => foldMessages(displayMessages),
    [displayMessages],
  );

  // ── Jump target ──

  const jumpTargetRef = useRef<"top" | "bottom" | null>(null);
  const [jumpVersion, setJumpVersion] = useState(0);
  const jumpTarget = jumpTargetRef.current;

  const clearJumpTarget = useCallback(() => {
    jumpTargetRef.current = null;
  }, []);

  // ── Ensure-renderable guard ──
  // Removed: ensureRenderable logic is now owned by useScrollController.

  // ── Pagination actions ──
  //
  // These read from useChatStore.getState() at CALL TIME, not from the
  // closure. This means the functions are always using the latest store
  // state, regardless of when React last rendered. Deps are minimal
  // (just agentId/sessionId which rarely change).
  //
  // ADR-050 (forward / oldest-end offset semantics):
  //   loadPrevPage → user scrolled UP near top → load OLDER messages.
  //                  nextOffset = max(0, messageOffset - limit).
  //                  No-op if !hasOlder || isLoadingMore.
  //   loadNextPage → user scrolled DOWN near bottom → load NEWER messages.
  //                  nextOffset = messageOffset + limit.
  //                  No-op if !hasNewer || isLoadingMore.

  const loadPrevPage = useCallback(async () => {
    if (!agentId || !sessionId) return;
    const ss = useChatStore.getState().getSessionState(agentId, sessionId);
    if (ss.isLoadingMore) return;
    // hasOlder: messageOffset > 0 means older messages remain above the window.
    if (!(ss.messageOffset > 0)) return;

    const nextOffset = Math.max(0, ss.messageOffset - PAGINATION_PAGE_SIZE);
    if (process.env.NODE_ENV === "development") {
      console.debug("[adapter:loadPrevPage]", {
        messageOffset: ss.messageOffset,
        messageLimit: ss.messageLimit,
        messageTotal: ss.messageTotal,
        nextOffset,
        pageSize: PAGINATION_PAGE_SIZE,
      });
    }
    setSessionLoadingMore(agentId, sessionId, true);
    try {
      await useChatStore.getState().loadSessionMessages(
        agentId, sessionId, nextOffset, PAGINATION_PAGE_SIZE,
      );
    } finally {
      setSessionLoadingMore(agentId, sessionId, false);
    }
  }, [agentId, sessionId]);

  const loadNextPage = useCallback(async () => {
    if (!agentId || !sessionId) return;
    const ss = useChatStore.getState().getSessionState(agentId, sessionId);
    if (ss.isLoadingMore) return;
    // hasNewer: messageOffset + messageLimit < messageTotal means newer messages remain below the window.
    if (!(ss.messageOffset + ss.messageLimit < ss.messageTotal)) return;

    const nextOffset = ss.messageOffset + ss.messageLimit;
    if (process.env.NODE_ENV === "development") {
      console.debug("[adapter:loadNextPage]", {
        messageOffset: ss.messageOffset,
        messageLimit: ss.messageLimit,
        messageTotal: ss.messageTotal,
        nextOffset,
        pageSize: PAGINATION_PAGE_SIZE,
      });
    }
    setSessionLoadingMore(agentId, sessionId, true);
    try {
      await useChatStore.getState().loadSessionMessages(
        agentId, sessionId, nextOffset, PAGINATION_PAGE_SIZE,
      );
    } finally {
      setSessionLoadingMore(agentId, sessionId, false);
    }
  }, [agentId, sessionId]);

  const jumpToLatest = useCallback(async () => {
    if (!agentId || !sessionId) return;
    await useChatStore.getState().ensureLatestInCache(agentId, sessionId);
    jumpTargetRef.current = "bottom";
    setJumpVersion((v) => v + 1);
  }, [agentId, sessionId]);

  const jumpToOldest = useCallback(async () => {
    if (!agentId || !sessionId) return;
    await useChatStore.getState().ensureOldestInCache(agentId, sessionId);
    jumpTargetRef.current = "top";
    setJumpVersion((v) => v + 1);
  }, [agentId, sessionId]);

  // ── onLayout ──
  // Removed: ensureRenderable logic is now owned by useScrollController.
  // The controller checks totalHeight vs viewportHeight and calls
  // loadPrevPage/loadNextPage directly, with state machine guards.

  return useMemo<ChatListAdapter>(
    () => ({
      blocks, hasOlder, hasNewer, isLoading: isLoadingMore,
      messageOffset, messageLimit, messageTotal,
      loadPrevPage, loadNextPage, jumpToLatest, jumpToOldest,
      jumpTarget, clearJumpTarget,
    }),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [
      blocks, hasOlder, hasNewer, isLoadingMore,
      messageOffset, messageLimit, messageTotal,
      loadPrevPage, loadNextPage, jumpToLatest, jumpToOldest,
      jumpTarget, clearJumpTarget,
      jumpVersion,
    ],
  );
}
