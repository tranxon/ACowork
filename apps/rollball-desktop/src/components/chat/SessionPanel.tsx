import { useEffect } from "react";
import { useSessionStore } from "../../stores/sessionStore";
import { Brain, MessageSquarePlus, Clock, MessageCircle } from "lucide-react";
import { cn } from "../../lib/utils";

interface SessionPanelProps {
  agentId: string;
  onClose: () => void;
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

export function SessionPanel({ agentId, onClose, onOpenMemory }: SessionPanelProps) {
  const {
    sessions,
    currentSessionId,
    isLoading,
    fetchSessions,
    switchSession,
    createSession,
  } = useSessionStore();

  // Fetch sessions when panel opens or agent changes
  useEffect(() => {
    if (agentId) {
      void fetchSessions(agentId);
    }
  }, [agentId, fetchSessions]);

  // Close panel on Escape key
  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        onClose();
      }
    };
    document.addEventListener("keydown", handleKeyDown);
    return () => document.removeEventListener("keydown", handleKeyDown);
  }, [onClose]);

  const handleCreateSession = () => {
    void createSession(agentId);
  };

  const handleSwitchSession = (sessionId: string) => {
    switchSession(sessionId);
    onClose();
  };

  return (
    <div className="flex h-full flex-col bg-white dark:bg-zinc-900">
      {/* Header */}
      <div className="flex items-center justify-between border-b border-zinc-200 px-4 py-3 dark:border-zinc-800">
        <span className="text-sm font-semibold text-zinc-900 dark:text-zinc-100">
          Sessions
        </span>
      </div>

      {/* Memory entry */}
      <button
        onClick={() => {
          onClose();
          onOpenMemory();
        }}
        className="flex items-center gap-2.5 border-b border-zinc-200 px-4 py-3 text-left transition-colors hover:bg-zinc-50 dark:border-zinc-800 dark:hover:bg-zinc-800/50"
      >
        <Brain className="h-4 w-4 text-zinc-500 dark:text-zinc-400" />
        <div className="flex-1">
          <div className="text-xs font-medium text-zinc-700 dark:text-zinc-300">
            Memory
          </div>
          <div className="text-[10px] text-zinc-400 dark:text-zinc-500">
            Browse memory nodes & episodes
          </div>
        </div>
        <span className="text-xs text-zinc-400 dark:text-zinc-500">→</span>
      </button>

      {/* Session list */}
      <div className="flex-1 overflow-y-auto">
        {isLoading && sessions.length === 0 && (
          <div className="px-4 py-6 text-center text-xs text-zinc-400 dark:text-zinc-500">
            Loading sessions...
          </div>
        )}

        {!isLoading && sessions.length === 0 && (
          <div className="px-4 py-6 text-center text-xs text-zinc-400 dark:text-zinc-500">
            No sessions yet
          </div>
        )}

        <div className="divide-y divide-zinc-100 dark:divide-zinc-800">
          {sessions.map((session) => {
            const isActive = session.session_id === currentSessionId;
            return (
              <button
                key={session.session_id}
                onClick={() => handleSwitchSession(session.session_id)}
                className={cn(
                  "flex w-full flex-col gap-0.5 px-4 py-3 text-left transition-colors",
                  isActive
                    ? "bg-zinc-100 dark:bg-zinc-800/80"
                    : "hover:bg-zinc-50 dark:hover:bg-zinc-800/50",
                )}
              >
                <div className="flex items-center gap-2">
                  <MessageCircle className="h-3.5 w-3.5 shrink-0 text-zinc-400 dark:text-zinc-500" />
                  <span className="min-w-0 flex-1 truncate text-xs font-medium text-zinc-700 dark:text-zinc-300">
                    {session.title || "Untitled session"}
                  </span>
                  {session.status === "active" && (
                    <span className="shrink-0 rounded-full bg-emerald-100 px-1.5 py-0.5 text-[9px] font-medium text-emerald-700 dark:bg-emerald-900/30 dark:text-emerald-400">
                      Active
                    </span>
                  )}
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
            );
          })}
        </div>
      </div>

      {/* Footer: New conversation */}
      <div className="border-t border-zinc-200 p-3 dark:border-zinc-800">
        <button
          onClick={handleCreateSession}
          className="flex w-full items-center justify-center gap-1.5 rounded-lg bg-zinc-800 px-3 py-2 text-xs font-medium text-white transition-colors hover:bg-zinc-700 dark:bg-zinc-700 dark:hover:bg-zinc-600"
        >
          <MessageSquarePlus className="h-3.5 w-3.5" />
          New Conversation
        </button>
      </div>
    </div>
  );
}
