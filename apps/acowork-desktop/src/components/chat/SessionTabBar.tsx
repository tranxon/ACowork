import { useState, useRef, useEffect } from "react";
import { useTranslation } from "../../i18n/useTranslation";
import { useSessionStore } from "../../stores/sessionStore";
import { useChatStore } from "../../stores/chatStore";
import { useDebugStore } from "../../stores/debugStore";
import { isSessionActive } from "../../lib/types";
import { cn } from "../../lib/utils";
import { Plus, Clock, Loader2, X, MessageCircle, Trash2, ChevronLeft, ChevronRight, Search, TriangleAlert } from "lucide-react";
import { StyledInput } from "../common/StyledInput";
import { ScrollableTabBar, type ScrollableTabBarHandle } from "../common/ScrollableTabBar";
import { TabItem } from "../common/tab";
import { Tooltip } from "../common/Tooltip";

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
  const { sessions, fetchSessions, switchSession, deleteSession, totalCount, currentPage, totalPages, pageSize } = useSessionStore();
  const openSessionIds = useChatStore((s) => s.agentStates[agentId]?.openSessionIds ?? []);
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

  const handleSelect = async (sessionId: string) => {
    await switchSession(sessionId, agentId);
    useDebugStore.getState().setCurrentSessionId(sessionId);
    useSessionStore.getState().saveSessionForAgent(agentId, sessionId);
    // Ensure tab is opened
    useChatStore.getState().openTab(agentId, sessionId);
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
      className="absolute right-0 top-full mt-1 w-72 rounded-lg border border-zinc-200 bg-white shadow-lg dark:border-zinc-700 dark:bg-zinc-800 z-50"
    >
      {/* Header with total count */}
      <div className="flex items-center justify-between border-b border-zinc-200 px-3 py-1.5 text-[11px] text-zinc-500 dark:border-zinc-700 dark:text-zinc-400">
        <span>
          {totalCount > 0 ? (
            <>Showing {start}&ndash;{end} of {totalCount}</>
          ) : (
            <>No sessions</>
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
            placeholder="Search sessions..."
            className="pl-7"
          />
        </div>
      </div>

      <div className="max-h-80 overflow-y-auto py-1">
        {filteredSessions.length === 0 && (
          <div className="px-3 py-4 text-center text-xs text-zinc-400 dark:text-zinc-500">
            No sessions yet
          </div>
        )}

        {filteredSessions.map((session) => {
          const isOpen = openSessionIds.includes(session.session_id);
          const isDeleting = confirmDelete === session.session_id;
          const sessionState = useChatStore.getState().getSessionState(agentId, session.session_id);
          const isActive = isSessionActive(sessionState?.sessionStatus);

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
                    className="rounded-md btn-accent px-2 py-0.5 text-xs disabled:opacity-50"
                  >
                    {t("common.delete")}
                  </button>
                  <button
                    onClick={(e) => { e.stopPropagation(); setConfirmDelete(null); }}
                    className="rounded-md btn-solid px-2 py-0.5 text-xs"
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
  const sessions = useSessionStore((s) => s.sessions);
  const { switchSession, createSession, saveSessionForAgent, closeSession } = useSessionStore();

  const [listOpen, setListOpen] = useState(false);
  const [closingSessionId, setClosingSessionId] = useState<string | null>(null);
  const scrollableRef = useRef<ScrollableTabBarHandle>(null);

  // Get title for a session
  const getTitle = (sessionId: string): string => {
    const session = sessions.find((s) => s.session_id === sessionId);
    return session?.title || "Untitled";
  };

  // Get status for a session tab
  const getStatus = (sessionId: string) => {
    const state = useChatStore.getState().getSessionState(agentId, sessionId);
    return state?.sessionStatus;
  };

  const handleTabClick = async (sessionId: string) => {
    // Ignore clicks that ended a drag
    if (scrollableRef.current?.hasMoved.current) return;
    if (sessionId === activeSessionId) return;
    await switchSession(sessionId, agentId);
    useDebugStore.getState().setCurrentSessionId(sessionId);
    saveSessionForAgent(agentId, sessionId);
  };

  const handleClose = async (e: React.MouseEvent, sessionId: string) => {
    e.stopPropagation();
    const status = getStatus(sessionId);

    // If session is looping, ask confirmation before closing
    if (isSessionActive(status)) {
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
    const newActiveId = useChatStore.getState().closeTab(agentId, sessionId);

    // If the closed tab was active, switch to the new active
    if (sessionId === activeSessionId && newActiveId) {
      switchSession(newActiveId, agentId);
      saveSessionForAgent(agentId, newActiveId);
    }

    // If no tabs remain, create a new session
    const remaining = useChatStore.getState().getOpenSessionIds(agentId);
    if (remaining.length === 0) {
      createSession(agentId);
    }
  };

  const handleNew = () => {
    createSession(agentId);
  };

  if (!agent) return null;

  return (
    <div className="flex items-center bg-[#FAFAFA] dark:bg-zinc-900 select-none px-0.5 gap-0.5 pt-[5px] border-b border-zinc-200 dark:border-zinc-800">
      <ScrollableTabBar
        ref={scrollableRef}
        activeItemSelector={activeSessionId ? `[data-session-id="${activeSessionId}"]` : undefined}
        activeItemId={activeSessionId ?? undefined}
      >
        {openSessionIds.map((sessionId) => {
          const isActive = sessionId === activeSessionId;
          const status = getStatus(sessionId);
          const isProcessing = isSessionActive(status);

          return (
            <TabItem
              data-session-id={sessionId}
              key={sessionId}
              onClick={() => handleTabClick(sessionId)}
              active={isActive}
            >
              {/* Streaming indicator dot (only when processing and not active) */}
              {isProcessing && !isActive && (
                <span className="shrink-0 h-1.5 w-1.5 rounded-full bg-zinc-400 dark:bg-zinc-500 animate-pulse" />
              )}
              {/* Title */}
              <span className={cn(
                "min-w-0 flex-1 truncate text-[length:var(--tab-font-size)] leading-[var(--tab-line-height)]",
                isProcessing && isActive && "text-zinc-700 dark:text-zinc-200",
              )}>
                {getTitle(sessionId)}
              </span>
              {/* Close button */}
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
            </TabItem>
          );
        })}
      </ScrollableTabBar>

      {/* Action buttons */}
      <div className="flex items-center shrink-0 px-1 gap-0.5">
        {/* New session button */}
        <Tooltip content={t("sessionTabBar.newConversation")} variant="plain">
          <button
            onClick={handleNew}
            className="flex items-center justify-center rounded p-1 text-zinc-400 hover:bg-zinc-200 hover:text-zinc-600 dark:hover:bg-zinc-700 dark:hover:text-zinc-300 transition-colors"
          >
            <Plus className="h-3.5 w-3.5 shrink-0" />
          </button>
        </Tooltip>

        {/* Session list dropdown */}
        <div className="relative">
          <Tooltip content={t("sessionTabBar.sessionHistory")} variant="plain">
            <button
              onClick={() => setListOpen(!listOpen)}
              className={cn(
                "flex items-center justify-center rounded p-1 transition-colors",
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
        <div className="fixed inset-0 z-[60] flex items-center justify-center bg-black/50" onClick={() => setClosingSessionId(null)}>
          <div
            className="mx-4 w-full max-w-sm rounded-xl border border-zinc-200 bg-white p-5 shadow-xl dark:border-zinc-700 dark:bg-zinc-800"
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
                className="rounded-lg btn-solid px-3 py-1.5 text-xs"
              >
                {t("sessionTabBar.cancel")}
              </button>
              <button
                onClick={confirmClose}
                className="rounded-lg btn-accent px-3 py-1.5 text-xs"
              >
                {t("sessionTabBar.confirmClose")}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
