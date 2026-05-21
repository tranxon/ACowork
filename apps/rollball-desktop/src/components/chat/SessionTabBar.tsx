import { useState, useRef, useEffect, useCallback } from "react";
import { useSessionStore } from "../../stores/sessionStore";
import { useChatStore } from "../../stores/chatStore";
import { isSessionActive } from "../../lib/types";
import { cn } from "../../lib/utils";
import { Plus, Clock, Loader2, X, MessageCircle, Trash2, ChevronLeft, ChevronRight } from "lucide-react";

// ── Relative time formatter ──────────────────────────────────────────────

function formatRelativeTime(dateStr: string): string {
  const date = new Date(dateStr);
  const now = new Date();
  const diffSec = Math.floor((now.getTime() - date.getTime()) / 1000);
  const diffMin = Math.floor(diffSec / 60);
  const diffHour = Math.floor(diffMin / 60);
  const diffDay = Math.floor(diffHour / 24);

  if (diffSec < 60) return "刚刚";
  if (diffMin < 60) return `${diffMin}分钟前`;
  if (diffHour < 24) return `${diffHour}小时前`;
  if (diffDay < 30) return `${diffDay}天前`;
  return date.toLocaleDateString("zh-CN", { month: "short", day: "numeric" });
}

// ── SessionListDropdown ──────────────────────────────────────────────────

interface SessionListDropdownProps {
  agentId: string;
  onClose: () => void;
}

function SessionListDropdown({ agentId, onClose }: SessionListDropdownProps) {
  const { sessions, fetchSessions, switchSession, deleteSession, totalCount, currentPage, totalPages, pageSize } = useSessionStore();
  const openSessionIds = useChatStore((s) => s.agentStates[agentId]?.openSessionIds ?? []);
  const [confirmDelete, setConfirmDelete] = useState<string | null>(null);
  const [deletingId, setDeletingId] = useState<string | null>(null);
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

  return (
    <div
      ref={ref}
      className="absolute left-0 top-full mt-1 w-72 rounded-lg border border-zinc-200 bg-white shadow-lg dark:border-zinc-700 dark:bg-zinc-800 z-50"
    >
      {/* Header with total count */}
      <div className="flex items-center justify-between border-b border-zinc-200 px-3 py-1.5 text-[11px] text-zinc-500 dark:border-zinc-700 dark:text-zinc-400">
        <span>
          {totalCount > 0 ? (
            <>Showing {start}–{end} of {totalCount}</>
          ) : (
            <>No sessions</>
          )}
        </span>
      </div>

      <div className="max-h-80 overflow-y-auto py-1">
        {sessions.length === 0 && (
          <div className="px-3 py-4 text-center text-xs text-zinc-400 dark:text-zinc-500">
            No sessions yet
          </div>
        )}

        {sessions.map((session) => {
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
                  <span>{formatRelativeTime(session.created_at)}</span>
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
                    删除
                  </button>
                  <button
                    onClick={(e) => { e.stopPropagation(); setConfirmDelete(null); }}
                    className="rounded-md btn-solid px-2 py-0.5 text-xs"
                  >
                    取消
                  </button>
                </div>
              ) : (
                <button
                  onClick={(e) => { e.stopPropagation(); setConfirmDelete(session.session_id); }}
                  disabled={deletingId !== null}
                  className="rounded p-1 text-zinc-400 opacity-0 transition-all group-hover:opacity-100 hover:bg-red-50 hover:text-red-600 dark:hover:bg-red-900/20 dark:hover:text-red-400 disabled:opacity-50"
                  title="Delete session"
                >
                  <Trash2 className="h-3 w-3" />
                </button>
              )}
            </div>
          );
        })}
      </div>

      {/* Pagination */}
      {totalPages > 1 && (
        <div className="flex items-center justify-between border-t border-zinc-200 px-3 py-1.5 dark:border-zinc-700">
          <button
            onClick={() => handlePageChange(currentPage - 1)}
            disabled={currentPage <= 1}
            className="inline-flex items-center rounded p-0.5 text-zinc-500 hover:bg-zinc-100 disabled:opacity-30 dark:text-zinc-400 dark:hover:bg-zinc-800"
          >
            <ChevronLeft className="h-3.5 w-3.5" />
          </button>
          <span className="text-[11px] text-zinc-500 dark:text-zinc-400">
            Page {currentPage} of {totalPages}
          </span>
          <button
            onClick={() => handlePageChange(currentPage + 1)}
            disabled={currentPage >= totalPages}
            className="inline-flex items-center rounded p-0.5 text-zinc-500 hover:bg-zinc-100 disabled:opacity-30 dark:text-zinc-400 dark:hover:bg-zinc-800"
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
  const agent = useChatStore((s) => s.agentStates[agentId]);
  const openSessionIds = agent?.openSessionIds ?? [];
  const activeSessionId = agent?.activeSessionId;
  const sessions = useSessionStore((s) => s.sessions);
  const { switchSession, createSession, saveSessionForAgent } = useSessionStore();

  const [listOpen, setListOpen] = useState(false);
  const scrollRef = useRef<HTMLDivElement>(null);
  const [canScrollLeft, setCanScrollLeft] = useState(false);
  const [canScrollRight, setCanScrollRight] = useState(false);

  // Drag-to-scroll state
  const isDragging = useRef(false);
  const dragStartX = useRef(0);
  const dragScrollLeft = useRef(0);
  const hasMoved = useRef(false);

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
    if (hasMoved.current) return;
    if (sessionId === activeSessionId) return;
    await switchSession(sessionId, agentId);
    saveSessionForAgent(agentId, sessionId);
  };

  const handleClose = (e: React.MouseEvent, sessionId: string) => {
    e.stopPropagation();
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

  // ── Scroll arrow logic ───────────────────────────────────────────────

  const updateScrollState = useCallback(() => {
    const el = scrollRef.current;
    if (!el) return;
    setCanScrollLeft(el.scrollLeft > 2);
    setCanScrollRight(el.scrollLeft + el.clientWidth < el.scrollWidth - 2);
  }, []);

  useEffect(() => {
    updateScrollState();
    const el = scrollRef.current;
    if (!el) return;
    el.addEventListener("scroll", updateScrollState, { passive: true });
    const ro = new ResizeObserver(updateScrollState);
    ro.observe(el);
    return () => {
      el.removeEventListener("scroll", updateScrollState);
      ro.disconnect();
    };
  }, [updateScrollState, openSessionIds.length]);

  const scrollBy = (dir: "left" | "right") => {
    scrollRef.current?.scrollBy({ left: dir === "left" ? -160 : 160, behavior: "smooth" });
  };

  // Scroll active tab into view
  useEffect(() => {
    if (!scrollRef.current || !activeSessionId) return;
    const activeEl = scrollRef.current.querySelector(`[data-session-id="${activeSessionId}"]`);
    activeEl?.scrollIntoView({ block: "nearest", inline: "nearest" });
  }, [activeSessionId]);

  // ── Drag-to-scroll ──────────────────────────────────────────────────

  const handleDragStart = useCallback((e: React.MouseEvent) => {
    const el = scrollRef.current;
    if (!el) return;
    isDragging.current = true;
    hasMoved.current = false;
    dragStartX.current = e.clientX;
    dragScrollLeft.current = el.scrollLeft;
    el.style.cursor = "grabbing";
    el.style.userSelect = "none";

    const onMouseMove = (ev: MouseEvent) => {
      if (!isDragging.current) return;
      const dx = ev.clientX - dragStartX.current;
      if (Math.abs(dx) > 3) hasMoved.current = true;
      el.scrollLeft = dragScrollLeft.current - dx;
    };

    const onMouseUp = () => {
      isDragging.current = false;
      el.style.cursor = "";
      el.style.userSelect = "";
      document.removeEventListener("mousemove", onMouseMove);
      document.removeEventListener("mouseup", onMouseUp);
    };

    document.addEventListener("mousemove", onMouseMove);
    document.addEventListener("mouseup", onMouseUp);
  }, []);

  if (!agent) return null;

  return (
    <div className="flex items-center bg-[#FAFAFA] dark:bg-zinc-900 select-none px-0.5 gap-0.5 mt-0.5 border-b border-zinc-200 dark:border-zinc-700">
      {/* Left scroll arrow */}
      {canScrollLeft && (
        <button
          onClick={() => scrollBy("left")}
          className="shrink-0 flex items-center justify-center w-5 h-full text-zinc-400 hover:text-zinc-600 hover:bg-zinc-200 dark:hover:bg-zinc-700 dark:hover:text-zinc-300 transition-colors"
        >
          <ChevronLeft className="h-3.5 w-3.5" />
        </button>
      )}

      {/* Scrollable tab area — drag-to-scroll enabled */}
      <div
        ref={scrollRef}
        className="flex flex-1 min-w-0 items-center overflow-x-auto gap-0.5 cursor-grab active:cursor-grabbing [&::-webkit-scrollbar]:hidden"
        style={{ scrollbarWidth: "none", msOverflowStyle: "none" }}
        onMouseDown={handleDragStart}
      >
        {openSessionIds.map((sessionId) => {
          const isActive = sessionId === activeSessionId;
          const status = getStatus(sessionId);
          const isProcessing = isSessionActive(status);

          return (
            <div
              key={sessionId}
              data-session-id={sessionId}
              onClick={() => handleTabClick(sessionId)}
              className={cn(
                "group relative flex items-center gap-1 pl-2.5 pr-1.5 py-1.5 min-w-[60px] max-w-[160px] cursor-pointer transition-colors shrink-0 border-b-2",
                isActive
                  ? "border-[var(--color-accent)] text-[var(--color-accent)]"
                  : "border-transparent text-zinc-500 dark:text-zinc-400 hover:text-zinc-700 dark:hover:text-zinc-300",
                isProcessing && !isActive && "text-[var(--color-accent)]",
              )}
              title={getTitle(sessionId)}
            >
              {/* Streaming indicator dot (only when processing and not active) */}
              {isProcessing && !isActive && (
                <span className="shrink-0 h-1.5 w-1.5 rounded-full bg-[var(--color-accent)] animate-pulse" />
              )}
              {/* Title */}
              <span className={cn(
                "min-w-0 flex-1 truncate",
                isProcessing && isActive && "text-[var(--color-accent)]",
              )} style={{ fontSize: "calc(var(--ui-font-size, 0.875rem) * 0.9)" }}>
                {getTitle(sessionId)}
              </span>
              {/* Close button */}
              <button
                onClick={(e) => handleClose(e, sessionId)}
                className={cn(
                  "shrink-0 rounded p-0.5 transition-opacity",
                  isActive ? "opacity-60 hover:opacity-100 hover:bg-zinc-200 dark:hover:bg-zinc-600" : "opacity-0 group-hover:opacity-60 hover:!opacity-100 hover:bg-zinc-300 dark:hover:bg-zinc-600",
                )}
                title="Close tab"
              >
                <X className="h-3 w-3" />
              </button>
            </div>
          );
        })}
      </div>

      {/* Right scroll arrow */}
      {canScrollRight && (
        <button
          onClick={() => scrollBy("right")}
          className="shrink-0 flex items-center justify-center w-5 h-full text-zinc-400 hover:text-zinc-600 hover:bg-zinc-200 dark:hover:bg-zinc-700 dark:hover:text-zinc-300 transition-colors"
        >
          <ChevronRight className="h-3.5 w-3.5" />
        </button>
      )}

      {/* Action buttons */}
      <div className="flex items-center shrink-0 px-1 gap-0.5">
        {/* New session button */}
        <button
          onClick={handleNew}
          className="rounded p-1 text-zinc-400 hover:bg-zinc-200 hover:text-zinc-600 dark:hover:bg-zinc-700 dark:hover:text-zinc-300 transition-colors"
          title="New conversation"
        >
          <Plus className="h-3.5 w-3.5" />
        </button>

        {/* Session list dropdown */}
        <div className="relative">
          <button
            onClick={() => setListOpen(!listOpen)}
            className={cn(
              "rounded p-1 transition-colors",
              listOpen
                ? "text-[var(--color-accent)] bg-zinc-200 dark:bg-zinc-700"
                : "text-zinc-400 hover:bg-zinc-200 hover:text-zinc-600 dark:hover:bg-zinc-700 dark:hover:text-zinc-300",
            )}
            title="Session history"
          >
            <Clock className="h-3.5 w-3.5" />
          </button>

          {listOpen && (
            <SessionListDropdown
              agentId={agentId}
              onClose={() => setListOpen(false)}
            />
          )}
        </div>
      </div>
    </div>
  );
}
