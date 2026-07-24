/**
 * useChatListAdapter - The single bridge between chatStore (data layer) and
 * VirtualMessageList (rendering layer).
 *
 * Design:
 *  - Folds raw ChatMessage[] into MessageBlock[] with stable, content-derived
 *    blockId (survives prepend/append).
 *  - loadBefore/loadAfter read state from useChatStore.getState() at call
 *    time, NOT from closure variables. This eliminates stale closures
 *    entirely - the function always sees the latest store state.
 *  - Pagination is triggered by a timer in ChatPanel (150ms interval),
 *    not by onScroll events or React effects. The timer reads scrollTop
 *    from the DOM and isLoading/hasOlder/hasNewer from the store.
 *  - Scroll position after prepend is maintained by VML's scrollHeight
 *    delta effect (classic infinite-scroll technique).
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
  loadBefore: () => Promise<void>;
  loadAfter: () => Promise<void>;
  jumpToLatest: () => Promise<void>;
  jumpToOldest: () => Promise<void>;
  readonly jumpTarget: "top" | "bottom" | null;
  clearJumpTarget: () => void;
  onLayout: (totalHeight: number, viewportHeight: number) => void;
}

// ── Constants ─────────────────────────────────────────────────────────────

const PAGINATION_PAGE_SIZE = 50;
const EMPTY_MESSAGES: ChatMessage[] = [];
const MAX_ENSURE_RENDERABLE_PAGES = 10;

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
  // loadBefore/loadAfter do NOT use these values - they read from
  // useChatStore.getState() at call time for the latest state.

  const messages = useChatStore((s) => {
    if (!agentId) return EMPTY_MESSAGES;
    const agent = s.agentStates[agentId];
    if (!agent) return EMPTY_MESSAGES;
    if (!sessionId) return EMPTY_MESSAGES;
    return agent.sessionStates[sessionId]?.messages ?? EMPTY_MESSAGES;
  });

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

  const hasOlder = messageOffset + messageLimit < messageTotal && messageLimit > 0;
  const hasNewer = messageOffset > 0;

  const blocks = useMemo<MessageBlock[]>(
    () => foldMessages(messages),
    [messages],
  );

  // ── Jump target ──

  const jumpTargetRef = useRef<"top" | "bottom" | null>(null);
  const [jumpVersion, setJumpVersion] = useState(0);
  const jumpTarget = jumpTargetRef.current;

  const clearJumpTarget = useCallback(() => {
    jumpTargetRef.current = null;
  }, []);

  // ── Ensure-renderable guard ──

  const ensureRenderableCountRef = useRef(0);

  // ── Pagination actions ──
  //
  // These read from useChatStore.getState() at CALL TIME, not from the
  // closure. This means the functions are always using the latest store
  // state, regardless of when React last rendered. Deps are minimal
  // (just agentId/sessionId which rarely change).

  const loadBefore = useCallback(async () => {
    if (!agentId || !sessionId) return;
    const ss = useChatStore.getState().getSessionState(agentId, sessionId);
    if (ss.isLoadingMore) return;
    if (ss.messageOffset + ss.messageLimit >= ss.messageTotal) return;

    const nextOffset = ss.messageOffset + ss.messageLimit;
    setSessionLoadingMore(agentId, sessionId, true);
    try {
      await useChatStore.getState().loadSessionMessages(
        agentId, sessionId, nextOffset, PAGINATION_PAGE_SIZE,
        { evictionDirection: "none" },
      );
    } finally {
      setSessionLoadingMore(agentId, sessionId, false);
    }
  }, [agentId, sessionId]);

  const loadAfter = useCallback(async () => {
    if (!agentId || !sessionId) return;
    const ss = useChatStore.getState().getSessionState(agentId, sessionId);
    if (ss.isLoadingMore) return;
    if (ss.messageOffset <= 0) return;

    const nextOffset = Math.max(0, ss.messageOffset - PAGINATION_PAGE_SIZE);
    setSessionLoadingMore(agentId, sessionId, true);
    try {
      await useChatStore.getState().loadSessionMessages(
        agentId, sessionId, nextOffset, PAGINATION_PAGE_SIZE,
        { evictionDirection: "none" },
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

  const onLayout = useCallback(
    (totalHeight: number, viewportHeight: number) => {
      if (!agentId || !sessionId) return;
      if (isLoadingMore) return;
      if (totalHeight >= viewportHeight) return;
      if (ensureRenderableCountRef.current >= MAX_ENSURE_RENDERABLE_PAGES) return;
      ensureRenderableCountRef.current += 1;
      if (hasNewer) { void loadAfter(); return; }
      if (hasOlder) { void loadBefore(); return; }
    },
    [agentId, sessionId, isLoadingMore, hasNewer, hasOlder, loadAfter, loadBefore],
  );

  return useMemo<ChatListAdapter>(
    () => ({
      blocks, hasOlder, hasNewer, isLoading: isLoadingMore,
      loadBefore, loadAfter, jumpToLatest, jumpToOldest,
      jumpTarget, clearJumpTarget, onLayout,
    }),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [
      blocks, hasOlder, hasNewer, isLoadingMore,
      loadBefore, loadAfter, jumpToLatest, jumpToOldest,
      jumpTarget, clearJumpTarget, onLayout,
      jumpVersion,
    ],
  );
}
