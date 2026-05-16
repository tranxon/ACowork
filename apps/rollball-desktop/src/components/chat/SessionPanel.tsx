import { useEffect, useState, useRef } from "react";
import { useSessionStore } from "../../stores/sessionStore";
import { Brain, MessageSquarePlus, Clock, MessageCircle, ChevronDown, Loader2, Trash2 } from "lucide-react";
import { cn } from "../../lib/utils";

interface SessionPanelProps {
  agentId: string;
  onOpenMemory: () => void;
}

/** Format a date string to relative time in Chinese */
function formatRelativeTime(dateStr: string): string {
  const date = new Date(dateStr);
  const now = new Date();
  const diffMs = now.getTime() - date.getTime();
  const diffSec = Math.floor(diffMs / 1000);
  const diffMin = Math.floor(diffSec / 60);
  const diffHour = Math.floor(diffMin / 60);
  const diffDay = Math.floor(diffHour / 24);

  if (diffSec < 60) return "刚刚";
  if (diffMin < 60) return `${diffMin} 分钟前`;
  if (diffHour < 24) return `${diffHour} 小时前`;
  if (diffDay < 30) return `${diffDay} 天前`;
  return date.toLocaleDateString("zh-CN", { month: "short", day: "numeric" });
}

export function SessionPanel({ agentId, onOpenMemory }: SessionPanelProps) {
  const {
    sessions,
    currentSessionId,
    isLoading,
    fetchSessions,
    switchSession,
    createSession,
    deleteSession,
  } = useSessionStore();
  const [open, setOpen] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState<string | null>(null);
  const [deletingId, setDeletingId] = useState<string | null>(null);
  const ref = useRef<HTMLDivElement>(null);
  const listRef = useRef<HTMLDivElement>(null);

  // Fetch sessions when panel opens or agent changes
  useEffect(() => {
    if (agentId && open) {
      void fetchSessions(agentId);
    }
  }, [agentId, fetchSessions, open]);

  // Close on outside click
  useEffect(() => {
    if (!open) return;
    const handler = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [open]);

  // Force scrollbar recalculation when sessions change (fixes stale scrollbar after agent switch)
  useEffect(() => {
    if (listRef.current) {
      // Trigger reflow to recalculate scrollbar dimensions
      const el = listRef.current;
      el.style.overflow = 'hidden';
      void el.offsetHeight; // force reflow
      el.style.overflow = '';
    }
  }, [sessions]);

  const handleSwitchSession = async (sessionId: string) => {
    await switchSession(sessionId, agentId);
    useSessionStore.getState().saveSessionForAgent(agentId, sessionId);
    setOpen(false);
  };

  const handleDeleteSession = async (sessionId: string) => {
    if (deletingId) return;
    setDeletingId(sessionId);
    try {
      await deleteSession(agentId, sessionId);
    } finally {
      setDeletingId(null);
      setConfirmDelete(null);
    }
  };

  return (
    <div ref={ref} className="relative">
      {/* Trigger button */}
      <button
        onClick={() => setOpen(!open)}
        className="inline-flex items-center gap-1.5 rounded-md px-2 py-1.5 text-xs transition-colors text-zinc-500 hover:bg-zinc-200 dark:hover:bg-zinc-700 hover:text-zinc-700 dark:hover:text-zinc-200"
      >
        <MessageCircle size={14} />
        Session
        <ChevronDown className={cn("h-3 w-3 transition-transform", open && "rotate-180")} />
      </button>

      {/* Dropdown menu */}
      {open && (
        <div className="absolute bottom-full left-0 mb-2 w-72 rounded-lg border border-zinc-200 bg-white shadow-lg dark:border-zinc-700 dark:bg-zinc-800 z-50">
          {/* Header */}
          <div className="flex items-center justify-between border-b border-zinc-200 px-3 py-2 dark:border-zinc-700">
            <span className="text-xs font-semibold text-zinc-900 dark:text-zinc-100">
              Sessions
            </span>
            <button
              onClick={() => {
                setOpen(false);
                onOpenMemory();
              }}
              className="flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px] text-zinc-500 hover:text-zinc-700 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:text-zinc-300 dark:hover:bg-zinc-700"
            >
              <Brain className="h-3 w-3" />
              Memory
            </button>
          </div>

          {/* Session list */}
          <div ref={listRef} className="max-h-80 overflow-y-auto py-1">
            {isLoading && sessions.length === 0 && (
              <div className="flex items-center justify-center gap-2 px-3 py-6 text-xs text-zinc-400 dark:text-zinc-500">
                <Loader2 className="h-3.5 w-3.5 animate-spin" />
                Loading sessions...
              </div>
            )}

            {!isLoading && sessions.length === 0 && (
              <div className="px-3 py-4 text-center text-xs text-zinc-400 dark:text-zinc-500">
                No sessions yet
              </div>
            )}

            {sessions.map((session) => {
              const isActive = session.session_id === currentSessionId;
              const isDeleting = confirmDelete === session.session_id;
              return (
                <div
                  key={session.session_id}
                  className="group flex items-center gap-2 px-3 py-2 transition-colors hover:bg-zinc-50 dark:hover:bg-zinc-700/50"
                >
                  {/* Select session button */}
                  <button
                    onClick={() => handleSwitchSession(session.session_id)}
                    className="flex min-w-0 flex-1 flex-col gap-0.5 text-left"
                  >
                    <div className="flex items-center gap-2">
                      <MessageCircle className="h-3.5 w-3.5 shrink-0 text-zinc-400 dark:text-zinc-500" />
                      <span className={cn("min-w-0 flex-1 truncate text-xs", isActive && "font-semibold text-accent-green", !isActive && "text-zinc-700 dark:text-zinc-300")}>
                        {session.title || "Untitled session"}
                      </span>
                    </div>
                    <div className="ml-5.5 flex items-center gap-2 text-[10px] text-zinc-400 dark:text-zinc-500">
                      <span className="flex items-center gap-1">
                        <Clock className="h-3 w-3" />
                        {formatRelativeTime(session.created_at)}
                      </span>
                      <span>·</span>
                      <span>{session.message_count} messages</span>
                    </div>
                  </button>

                  {/* Delete button */}
                  {isDeleting ? (
                    <div className="flex items-center gap-1">
                      <button
                        onClick={(e) => {
                          e.stopPropagation();
                          void handleDeleteSession(session.session_id);
                        }}
                        disabled={deletingId !== null}
                        className="rounded bg-red-500 px-2 py-0.5 text-xs text-white hover:bg-red-600 disabled:opacity-50 disabled:cursor-not-allowed"
                      >
                        删除
                      </button>
                      <button
                        onClick={(e) => {
                          e.stopPropagation();
                          setConfirmDelete(null);
                        }}
                        className="rounded bg-zinc-200 px-2 py-0.5 text-xs text-zinc-600 hover:bg-zinc-300 dark:bg-zinc-600 dark:text-zinc-300 dark:hover:bg-zinc-500"
                      >
                        取消
                      </button>
                    </div>
                  ) : (
                    <button
                      onClick={(e) => {
                        e.stopPropagation();
                        setConfirmDelete(session.session_id);
                      }}
                      disabled={deletingId !== null}
                      className="rounded p-1 text-zinc-400 opacity-0 transition-all group-hover:opacity-100 hover:bg-red-50 hover:text-red-600 dark:hover:bg-red-900/20 dark:hover:text-red-400 disabled:opacity-50 disabled:cursor-not-allowed"
                      title="Delete session"
                    >
                      <Trash2 className="h-3 w-3" />
                    </button>
                  )}
                </div>
              );
            })}
          </div>

          {/* Footer: New conversation */}
          <div className="border-t border-zinc-200 p-2 dark:border-zinc-700">
            <button
              onClick={() => {
                createSession(agentId);
                setOpen(false);
              }}
              className="mx-1.5 flex w-[calc(100%-0.75rem)] items-center justify-center gap-1.5 rounded-md bg-zinc-100 px-3 py-2 text-xs font-medium text-zinc-700 transition-colors hover:bg-zinc-200 dark:bg-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-600"
            >
              <MessageSquarePlus className="h-3.5 w-3.5" />
              New Conversation
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
