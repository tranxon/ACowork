import { FileText, Folder, X, Hash } from "lucide-react";
import { useAgentStore } from "../../stores/agentStore";
import { useChatStore } from "../../stores/chatStore";

/** Stable empty array reference to avoid Zustand infinite re-renders */
const EMPTY_CTX: never[] = [];

/** Chips showing files/directories attached to chat context (from right-click "Add to Chat") */
export function AttachedContextChips() {
  const selectedAgentId = useAgentStore((s) => s.selectedAgentId);

  const attachedContext = useChatStore((s) => {
    if (!selectedAgentId) return EMPTY_CTX;
    const agent = s.agentStates[selectedAgentId];
    if (!agent?.activeSessionId) return EMPTY_CTX;
    const ss = agent.sessionStates[agent.activeSessionId];
    return ss?.attachedContext ?? EMPTY_CTX;
  });

  const removeAttachedContext = useChatStore((s) => s.removeAttachedContext);

  if (!selectedAgentId || attachedContext.length === 0) return null;

  return (
    <div className="flex flex-wrap items-center gap-1.5 px-3 pt-2">
      {attachedContext.map((item) => (
        <div
          key={item.id}
          className="inline-flex items-center gap-1 rounded-md border border-zinc-200 bg-zinc-50 px-2 py-0.5 text-xs text-zinc-700 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-300"
        >
          {item.type === "directory" ? (
            <Folder className="h-3 w-3 shrink-0 text-amber-500" />
          ) : item.type === "selection" ? (
            <Hash className="h-3 w-3 shrink-0 text-[var(--color-accent)]" />
          ) : (
            <FileText className="h-3 w-3 shrink-0 text-zinc-400" />
          )}
          <span className="max-w-[200px] truncate">{item.name}</span>
          <button
            type="button"
            className="ml-0.5 rounded p-0.5 text-zinc-400 hover:bg-zinc-200 hover:text-zinc-600 dark:hover:bg-zinc-700 dark:hover:text-zinc-300"
            onClick={() => {
              const agentId = useAgentStore.getState().selectedAgentId;
              if (!agentId) return;
              const sessionId = useChatStore.getState().getActiveSessionId(agentId);
              if (!sessionId) return;
              removeAttachedContext(agentId, sessionId, item.id);
            }}
            aria-label={`Remove ${item.name}`}
          >
            <X className="h-3 w-3" />
          </button>
        </div>
      ))}
    </div>
  );
}
