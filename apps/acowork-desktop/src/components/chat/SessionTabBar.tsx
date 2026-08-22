import { useState, useRef, useEffect, useMemo } from "react";
import { useTranslation } from "../../i18n/useTranslation";
import { useAgentStore } from "../../stores/agentStore";
import { useChatStore } from "../../stores/chatStore";
import { isProcessing } from "../../lib/types";
import { cn } from "../../lib/utils";
import {
  ContextMenu,
  useContextMenu,
  type ContextMenuItem,
} from "../common/ContextMenu";
import { Plus, Clock, Loader2, X, MessageCircle, Trash2, ChevronLeft, ChevronRight, Search, TriangleAlert, XSquare } from "lucide-react";
import { StyledInput } from "../common/StyledInput";
import { ScrollableTabBar, type ScrollableTabBarHandle } from "../common/ScrollableTabBar";
import { TabItem } from "../common/tab";
import { Tooltip } from "../common/Tooltip";

const EMPTY_ARRAY: string[] = [];

// ── Relative time formatter ──────────────────────────────────────────────

function formatRelativeTime(dateStr: string, t: (key: string, options?: Record<string, unknown>) => string): string {
  const date = new Date(dateStr);
  const now = new Date();
  const diffSec = Math.floor((now.getTime() - date.getTime()) / 1000);
  const diffMin = Math.floor(diffSec / 60);
  const diffHour = Math.floor(diffMin / 60);
  const diffDay = Math.floor(diffHour / 24);

  if (diffSec < 60) return t("time.justNow");
  if (diffMin < 60) return t("time.minutesAgo", { count: diffMin });
  if (diffHour < 24) return t("time.hoursAgo", { count: diffHour });
  if (diffDay < 30) return t("time.daysAgo", { count: diffDay });
  return date.toLocaleDateString("en", { month: "short", day: "numeric" });
}

// ── SessionListDropdown ──────────────────────────────────────────────────

interface SessionListDropdownProps {
  agentId: string;
  onClose: () => void;
}

function SessionListDropdown({ agentId, onClose }: SessionListDropdownProps) {
  const { t } = useTranslation();
  const agentStorage = useAgentStore((s) => s.agents[agentId]);
  const sessions = agentStorage?.sessions ?? [];
  const totalCount = agentStorage?.pagination.totalCount ?? 0;
  const currentPage = agentStorage?.pagination.currentPage ?? 1;
  const totalPages = agentStorage?.pagination.totalPages ?? 1;
  const pageSize = agentStorage?.pagination.pageSize ?? 20;
  const fetchSessions = useAgentStore((s) => s.fetchSessions);
  const deleteSession = useAgentStore((s) => s.deleteSession);
  const openSessionIds = useChatStore((s) => s.agentStates[agentId]?.openSessionIds ?? EMPTY_ARRAY);
  const [confirmDelete, setConfirmDelete] = useState<string | null>(null);
  const [deletingId, setDeletingId] = useState<string | null>(null);
  const [searchTerm, setSearchTerm] = useState("");
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    void fetchSessions(agentId, 1);
  }, [agentId, fetchSessions]);

  // Close on outside click
  useEffect(() => {
    const handler = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) onClose();
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [onClose]);

  // ADR-038: selecting a session from the history dropdown is a "first-open"
  // event from the user's POV — it may or may not be in `openSessionIds`.
  // We delegate to `chatStore.openSession`, which combines the UI half
  // (open tab) with the backend half (MQTT open_session + HTTP reload).
  const handleSelect = async (sessionId: string) => {
    await useChatStore.getState().openSession(agentId, sessionId);
    onClose();
  };

  const handleDelete = async (sessionId: string) => {
    if (deletingId) return;
    setDeletingId(sessionId);
    try {
      await deleteSession(agentId, sessionId);
      // Also close the tab if open
      if (openSessionIds.includes(sessionId)) {
        useChatStore.getState().closeTab(agentId, sessionId);
      }
    } finally {
      setDeletingId(null);
      setConfirmDelete(null);
    }
  };

  const handlePageChange = (page: number) => {
    void fetchSessions(agentId, page);
  };

  const start = (currentPage - 1) * pageSize + 1;
  const end = Math.min(currentPage * pageSize, totalCount);

  // Client-side search filter
  const filteredSessions = searchTerm.trim()
    ? sessions.filter((s) => s.title?.toLowerCase().includes(searchTerm.toLowerCase()))
    : sessions;

  return (
    <div
      ref={ref}
      className="absolute right-0 top-full mt-1 w-72 rounded-md border border-zinc-200 bg-modal-surface shadow-lg dark:border-zinc-700 z-50"
    >
      {/* Header with total count */}
      <div className="flex items-center justify-between border-b border-zinc-200 px-3 py-1.5 text-[11px] text-zinc-500 dark:border-zinc-700 dark:text-zinc-400">
        <span>
          {totalCount > 0 ? (
            <>{t("sessionTabBar.sidebarShowing", { start, end, total: totalCount })}</>
          ) : (
            <>{t("sessionTabBar.sidebarNoSessions")}</>
          )}
        </span>
      </div>

      {/* Search input */}
      <div className="border-b border-zinc-200 px-2 py-1.5 dark:border-zinc-700">
        <div className="relative">
          <Search className="pointer-events-none absolute left-2 top-1/2 -translate-y-1/2 h-3 w-3 text-zinc-400" />
          <StyledInput
            type="text"
            value={searchTerm}
            onChange={(e) => setSearchTerm(e.target.value)}
            placeholder={t("sessionTabBar.sidebarSearchPlaceholder")}
            className="pl-7"
          />
        </div>
      </div>

      <div className="max-h-80 overflow-y-auto py-1">
        {filteredSessions.length === 0 && (
          <div className="px-3 py-4 text-center text-xs text-zinc-400 dark:text-zinc-500">
            {t("sessionTabBar.sidebarNoSessionsYet")}
          </div>
        )}

        {filteredSessions.map((session) => {
          const isOpen = openSessionIds.includes(session.session_id);
          const isDeleting = confirmDelete === session.session_id;
          const sessionState = useChatStore.getState().getSessionState(agentId, session.session_id);
          const isActive = isProcessing(sessionState?.sessionStatus);

          return (
            <div
              key={session.session_id}
              className="group flex items-center gap-2 px-3 py-2 transition-colors hover:bg-zinc-50 dark:hover:bg-zinc-700/50"
            >
              <button
                onClick={() => handleSelect(session.session_id)}
                className="flex min-w-0 flex-1 flex-col gap-0.5 text-left"
              >
                <div className="flex items-center gap-2">
                  {isActive ? (
                    <Loader2 className="h-3.5 w-3.5 shrink-0 animate-spin text-[var(--color-accent)]" />
                  ) : (
                    <MessageCircle className="h-3.5 w-3.5 shrink-0 text-zinc-400 dark:text-zinc-500" />
                  )}
                  <span className={cn("min-w-0 flex-1 truncate text-xs text-zinc-700 dark:text-zinc-300")}>
                    {session.title || "Untitled session"}
                  </span>
                  {isOpen && (
                    <span className="h-1.5 w-1.5 shrink-0 rounded-full bg-[var(--color-accent)]" />
                  )}
                </div>
                <div className="ml-5.5 flex items-center gap-2 text-[10px] text-zinc-400 dark:text-zinc-500">
                  <span>{formatRelativeTime(session.created_at, t)}</span>
                  <span>·</span>
                  <span>{session.message_count} msg</span>
                </div>
              </button>

              {isDeleting ? (
                <div className="flex items-center gap-1">
                  <button
                    onClick={(e) => { e.stopPropagation(); void handleDelete(session.session_id); }}
                    disabled={deletingId !== null}
                    className="rounded btn-accent px-2 py-0.5 text-xs disabled:opacity-50"
                  >
                    {t("common.delete")}
                  </button>
                  <button
                    onClick={(e) => { e.stopPropagation(); setConfirmDelete(null); }}
                    className="rounded btn-solid px-2 py-0.5 text-xs"
                  >
                    {t("common.cancel")}
                  </button>
                </div>
              ) : (
                <Tooltip content={t("sessionTabBar.deleteSession")} variant="plain">
                  <button
                    onClick={(e) => { e.stopPropagation(); setConfirmDelete(session.session_id); }}
                    disabled={deletingId !== null}
                    className="rounded p-1 text-zinc-400 opacity-0 transition-all group-hover:opacity-100 hover:bg-red-50 hover:text-red-600 dark:hover:bg-red-900/20 dark:hover:text-red-400 disabled:opacity-50"
                  >
                    <Trash2 className="h-3 w-3" />
                  </button>
                </Tooltip>
              )}
            </div>
          );
        })}
      </div>

      {/* Pagination */}
      {totalPages > 1 && (
        <div className="flex items-center justify-between border-t border-zinc-200 px-1 py-1.5 dark:border-zinc-700">
          <button
            onClick={() => handlePageChange(currentPage - 1)}
            disabled={currentPage <= 1}
            className="inline-flex items-center rounded-md px-1.5 py-0.5 text-zinc-500 hover:bg-zinc-100 disabled:opacity-30 dark:text-zinc-400 dark:hover:bg-zinc-800"
          >
            <ChevronLeft className="h-3.5 w-3.5" />
          </button>
          <span className="text-[11px] text-zinc-500 dark:text-zinc-400">
            Page {currentPage} of {totalPages}
          </span>
          <button
            onClick={() => handlePageChange(currentPage + 1)}
            disabled={currentPage >= totalPages}
            className="inline-flex items-center rounded-md px-1.5 py-0.5 text-zinc-500 hover:bg-zinc-100 disabled:opacity-30 dark:text-zinc-400 dark:hover:bg-zinc-800"
          >
            <ChevronRight className="h-3.5 w-3.5" />
          </button>
        </div>
      )}
    </div>
  );
}

// ── SessionTabBar ────────────────────────────────────────────────────────

interface SessionTabBarProps {
  agentId: string;
}

export function SessionTabBar({ agentId }: SessionTabBarProps) {
  const { t } = useTranslation();
  const agent = useChatStore((s) => s.agentStates[agentId]);
  const openSessionIds = agent?.openSessionIds ?? [];
  const activeSessionId = agent?.activeSessionId;
  const sessions = useAgentStore((s) => s.agents[agentId]?.sessions ?? []);
  const { createSession, closeSession } = useAgentStore();
  const setActiveTab = useChatStore((s) => s.setActiveTab);
  const openSession = useChatStore((s) => s.openSession);

  const [listOpen, setListOpen] = useState(false);
  const [closingSessionId, setClosingSessionId] = useState<string | null>(null);
  // Right-click context menu on session tabs. Payload is the sessionId of
  // the tab the user right-clicked. Outside-click / Escape / portal
  // positioning are all owned by `useContextMenu`.
  const tabMenu = useContextMenu<{ sessionId: string }>();
  const scrollableRef = useRef<ScrollableTabBarHandle>(null);

  // Get title for a session
  const getTitle = (sessionId: string): string => {
    const session = sessions.find((s) => s.session_id === sessionId);
    return session?.title || t("sessionTabBar.untitled");
  };

  // Get status for a session tab
  const getStatus = (sessionId: string) => {
    const state = useChatStore.getState().getSessionState(agentId, sessionId);
    return state?.sessionStatus;
  };

  // ADR-038: clicking on a tab in the strip is "切换前台" (switching foreground)
  // — the session is *already* in `openSessionIds` and has been confirmed
  // Active on the backend. We use the strict no-side-effect `setActiveTab`.
  const handleTabClick = (sessionId: string) => {
    // Ignore clicks that ended a drag
    if (scrollableRef.current?.hasMoved.current) return;
    if (sessionId === activeSessionId) return;
    setActiveTab(agentId, sessionId);
  };

  const handleClose = async (e: React.MouseEvent, sessionId: string) => {
    e.stopPropagation();
    const status = getStatus(sessionId);

    // If session is looping, ask confirmation before closing
    if (isProcessing(status)) {
      setClosingSessionId(sessionId);
      return;
    }

    await closeSession(agentId, sessionId);
    finishCloseTab(sessionId);
  };

  const confirmClose = async () => {
    if (!closingSessionId) return;
    const sid = closingSessionId;
    setClosingSessionId(null);
    await closeSession(agentId, sid);
    finishCloseTab(sid);
  };

  const finishCloseTab = (sessionId: string) => {
    void useChatStore.getState().closeTab(agentId, sessionId).then((newActiveId) => {
      // If the closed tab was active, switch to the new active
      if (sessionId === activeSessionId && newActiveId) {
        // ADR-038: the new active session is now in openSessionIds (the
        // neighbor was the immediate right-or-left sibling). It is the
        // "still open" case, so `setActiveTab` is the right verb.
        useChatStore.getState().setActiveTab(agentId, newActiveId);
      }

      // If no tabs remain, reopen an existing session from the session list
      // instead of unconditionally creating a new one.  Creating always would
      // trigger an infinite loop: close last tab → auto-create 1-tab session →
      // close it → auto-create again.  Switching to a real session breaks the
      // cycle while still guaranteeing the chat area is never blank.
      const remaining = useChatStore.getState().getOpenSessionIds(agentId);
      if (remaining.length === 0) {
        const sessions = useAgentStore.getState().agents[agentId]?.sessions;
        const otherSession = sessions?.find((s: { session_id: string }) => s.session_id !== sessionId);
        if (otherSession) {
          // ADR-038: this session is not yet in openSessionIds (it was a
          // background session from the sidebar), so we need the full
          // openSession path (UI + backend).
          void openSession(agentId, otherSession.session_id);
        } else {
          createSession(agentId);
        }
      }
    });
  };

  const handleNew = () => {
    createSession(agentId);
  };

  // ── Session tab right-click menu ──────────────────────────────────────
  const handleTabContextMenu = (e: React.MouseEvent, sessionId: string) => {
    tabMenu.openAt(e, { sessionId });
  };

  // Close the right-clicked session itself. If it's currently looping,
  // defer to the existing confirm dialog; otherwise close immediately.
  const handleContextCloseChat = async (sessionId: string) => {
    const status = getStatus(sessionId);
    if (isProcessing(status)) {
      setClosingSessionId(sessionId);
      return;
    }
    await closeSession(agentId, sessionId);
    finishCloseTab(sessionId);
  };

  // Close every other open session, keeping the right-clicked one.
  // Mirrors VS Code's "Close Others" — runs sequentially to give each
  // session a chance to switch the active selection cleanly.
  const handleContextCloseOthers = async (keepSessionId: string) => {
    const others = openSessionIds.filter((id) => id !== keepSessionId);
    for (const id of others) {
      // Skip sessions currently streaming — closing them would interrupt
      // a live response. The user can right-click them individually to
      // get the explicit confirm dialog.
      const status = getStatus(id);
      if (isProcessing(status)) continue;
      await closeSession(agentId, id);
    }
  };

  const handleContextOpenNewSession = () => {
    createSession(agentId);
  };

  // Memoised menu items. Built only when the right-clicked session or
  // the open-tab count changes (which gates the disabled flags).
  const tabMenuItems = useMemo<ContextMenuItem<{ sessionId: string }>[]>(() => {
    const sid = tabMenu.payload?.sessionId;
    if (!sid) return [];
    const canClose = openSessionIds.length > 1;
    const items: ContextMenuItem<{ sessionId: string }>[] = [
      {
        key: "close-chat",
        icon: <X size={14} />,
        label: t("sessionTabBar.closeChat"),
        disabled: !canClose,
        // Disable when only one tab remains — keep at least one open so
        // the chat area never ends up empty.
        onClick: ({ payload }) => payload && handleContextCloseChat(payload.sessionId),
      },
      {
        key: "close-others",
        icon: <XSquare size={14} />,
        label: t("sessionTabBar.closeOtherChats"),
        disabled: !canClose,
        // Same guard: "Close others" is a no-op when there are no others.
        onClick: ({ payload }) => payload && handleContextCloseOthers(payload.sessionId),
      },
      {
        key: "new-session",
        icon: <Plus size={14} />,
        label: t("sessionTabBar.openNewSession"),
        dividerBefore: true,
        onClick: handleContextOpenNewSession,
      },
    ];
    return items;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tabMenu.payload?.sessionId, openSessionIds.length, t]);

  if (!agent) return null;

  return (
    <div className="flex select-none px-0.5 gap-0.5 pt-[5px] border-b border-zinc-200 dark:border-zinc-800">
      <ScrollableTabBar
        ref={scrollableRef}
        activeItemSelector={activeSessionId ? `[data-session-id="${activeSessionId}"]` : undefined}
        activeItemId={activeSessionId ?? undefined}
      >
        {openSessionIds.map((sessionId) => {
          const isActive = sessionId === activeSessionId;
          const status = getStatus(sessionId);
          const isProc = isProcessing(status);

          return (
            <TabItem
              data-session-id={sessionId}
              key={sessionId}
              onClick={() => handleTabClick(sessionId)}
              onContextMenu={(e) => handleTabContextMenu(e, sessionId)}
              active={isActive}
            >
              {/* Streaming indicator dot (only when processing and not active) */}
              {isProc && !isActive && (
                <span className="shrink-0 h-1.5 w-1.5 rounded-full bg-zinc-400 dark:bg-zinc-500 animate-pulse" />
              )}
              {/* Title */}
              <span className={cn(
                "min-w-0 flex-1 truncate text-[length:var(--tab-font-size)] leading-[var(--tab-line-height)]",
                isProc && isActive && "text-zinc-700 dark:text-zinc-200",
              )}>
                {getTitle(sessionId)}
              </span>
              {/* Close button — hidden when this is the only remaining tab to
                  prevent a tab-less state. Open at least one more session to
                  close this one, matching common AI IDE conventions
                  (Cursor / Claude Code). */}
              {openSessionIds.length > 1 && (
                <Tooltip content={t("sessionTabBar.closeTab")} variant="plain">
                  <button
                    onClick={(e) => handleClose(e, sessionId)}
                    className={cn(
                      "shrink-0 rounded p-0.5 transition-opacity",
                      isActive ? "opacity-60 hover:opacity-100 hover:bg-zinc-200 dark:hover:bg-zinc-600" : "opacity-0 group-hover:opacity-60 hover:!opacity-100 hover:bg-zinc-300 dark:hover:bg-zinc-600",
                    )}
                  >
                    <X className="h-3 w-3" />
                  </button>
                </Tooltip>
              )}
            </TabItem>
          );
        })}
      </ScrollableTabBar>

      {/* Action buttons. Container shape (`gap-1 pr-2 shrink-0`) mirrors the
          Locate/Save wrapper in FileEditorPanel so that:
          - Button-to-button gap = 4px (gap-1) on both tab bars
          - Right-most button → panel right edge = 8px (pr-2) + 2px (parent
            px-0.5) = 10px on both tab bars
          Previously this container used `px-1 gap-0.5`, which placed the
          🕐 button only 6px from the right edge and left the two +/🕐
          buttons visually closer than their FileEditorPanel cousins. */}
      <div className="flex items-center gap-1 pr-2 shrink-0">
        {/* New session button. Wrapped in `relative inline-flex` to exactly
            match the +/🕐 siblings below — without the parity, the un-wrapped
            Tooltip wrapper (itself `relative inline-flex`) sits at a slightly
            different baseline than a block-level wrapper, and items-center
            can't fully cancel that micro-offset, leaving the + button visibly
            lower than the 🕐 button. */}
        <div className="relative inline-flex">
          <Tooltip content={t("sessionTabBar.newConversation")} variant="plain">
            <button
              onClick={handleNew}
              className="inline-flex items-center justify-center rounded h-6 w-6 text-zinc-400 hover:bg-zinc-200 hover:text-zinc-600 dark:hover:bg-zinc-700 dark:hover:text-zinc-300 transition-colors"
            >
              <Plus className="h-3.5 w-3.5 shrink-0" />
            </button>
          </Tooltip>
        </div>

        {/* Session list dropdown
            The outer wrapper must stay `relative` so the absolute-positioned
            SessionListDropdown anchors here. We add `inline-flex` to match
            the wrapper that wrapping Tooltip provides for the + button
            (`<div class="relative inline-flex">`) — without this parity, the
            block-level wrapper has subtly different line-height / baseline
            behavior, which `items-center` cannot fully cancel and causes the
            🕐 button to sit a hair higher than the + button. */}
        <div className="relative inline-flex">
          <Tooltip content={t("sessionTabBar.sessionHistory")} variant="plain">
            <button
              onClick={() => setListOpen(!listOpen)}
              className={cn(
                "inline-flex items-center justify-center rounded h-6 w-6 transition-colors",
                listOpen
                  ? "text-[var(--color-accent)] bg-zinc-200 dark:bg-zinc-700"
                  : "text-zinc-400 hover:bg-zinc-200 hover:text-zinc-600 dark:hover:bg-zinc-700 dark:hover:text-zinc-300",
              )}
            >
              <Clock className="h-3.5 w-3.5" />
            </button>
          </Tooltip>

          {listOpen && (
            <SessionListDropdown
              agentId={agentId}
              onClose={() => setListOpen(false)}
            />
          )}
        </div>
      </div>

      {/* Close confirmation dialog for looping sessions */}
      {closingSessionId && (
        <div className="fixed inset-0 z-[60] flex items-center justify-center bg-modal-overlay" onClick={() => setClosingSessionId(null)}>
          <div
            className="mx-4 w-full max-w-sm rounded-md border border-zinc-200 bg-modal-surface p-5 shadow-xl dark:border-zinc-700"
            onClick={(e) => e.stopPropagation()}
          >
            <div className="flex items-start gap-3">
              <div className="flex h-9 w-9 shrink-0 items-center justify-center rounded-full bg-amber-100 dark:bg-amber-900/30">
                <TriangleAlert className="h-5 w-5 text-amber-600 dark:text-amber-400" />
              </div>
              <div className="flex-1">
                <h3 className="text-sm font-medium text-zinc-800 dark:text-zinc-200">
                  {t("sessionTabBar.llmReasoning")}
                </h3>
                <p className="mt-1 text-xs text-zinc-500 dark:text-zinc-400">
                  {t("sessionTabBar.closeWarning")}
                </p>
              </div>
            </div>
            <div className="mt-4 flex justify-end gap-2">
              <button
                onClick={() => setClosingSessionId(null)}
                className="rounded btn-solid px-3 py-1.5 text-xs"
              >
                {t("sessionTabBar.cancel")}
              </button>
              <button
                onClick={confirmClose}
                className="rounded btn-accent px-3 py-1.5 text-xs"
              >
                {t("sessionTabBar.confirmClose")}
              </button>
            </div>
          </div>
        </div>
      )}

      {/* Session tab right-click context menu — uses global .context-menu classes */}
      <ContextMenu<{ sessionId: string }>
        isOpen={tabMenu.isOpen}
        menuProps={tabMenu.menuProps}
        items={tabMenuItems}
        payload={tabMenu.payload}
        selectionAtOpen={tabMenu.selectionAtOpen}
        onClose={tabMenu.close}
      />
    </div>
  );
}
