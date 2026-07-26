import { FileText, Folder, X, Hash } from "lucide-react";
import { useAgentStore } from "../../stores/agentStore";
import { useChatStore } from "../../stores/chatStore";
import { useWorkspaceStore } from "../../stores/workspaceStore";
import { openAttachedRef } from "../../lib/openWorkspaceRef";

/** Stable empty array reference to avoid Zustand infinite re-renders */
const EMPTY_CTX: never[] = [];

/**
 * Chips showing files/directories/selections attached to the chat input
 * (from right-click "Add to Chat" or programmatic `addAttachedContext`).
 *
 * Interaction:
 *   - Click the chip body → open the workspace ref in the in-app fileTab
 *     via `openAttachedRef`. `attached_folder` is intentionally a no-op
 *     (fileTab doesn't support folders yet); the chip still renders so
 *     users see what they attached and can remove it.
 *   - Click the X button → drop the chip from the pending attachment list
 *     via `removeAttachedContext`. The X is a real `<button>` (not nested
 *     in the body button) so we don't get an invalid
 *     `button-in-button` DOM structure.
 */
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

  // Derive the active session's workspace ID once per render — used as
  // the highest-priority candidate root by `resolveAssetAcrossWorkspaces`.
  const workspaceId = useWorkspaceStore.getState().getSessionWorkspaceId(
    useChatStore.getState().getActiveSessionId(selectedAgentId) ?? "",
  );

  return (
    <div className="flex flex-wrap items-center gap-1.5 px-3 pt-2">
      {attachedContext.map((item) => {
        const handleOpen = () => {
          void openAttachedRef({
            item,
            agentId: selectedAgentId,
            currentWorkspaceId: workspaceId,
          });
        };
        const handleRemove = (e: React.MouseEvent | React.KeyboardEvent) => {
          e.stopPropagation();
          const sessionId = useChatStore.getState().getActiveSessionId(selectedAgentId);
          if (!sessionId) return;
          removeAttachedContext(selectedAgentId, sessionId, item.id);
        };
        // Folders can't be opened by fileTab yet (silent no-op), so don't
        // pretend they're clickable — keep the body plain and let users
        // remove them via the X button. Files & selections get the full
        // pointer/keyboard affordance.
        const isOpenable = item.type === "file" || item.type === "selection";
        return (
          <div
            key={item.id}
            className="inline-flex items-center gap-1 rounded-md border border-zinc-200 bg-zinc-50 text-xs text-zinc-700 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-300"
          >
            {isOpenable ? (
              /* Clickable chip body — opens the workspace ref in fileTab.
               * `role="button"` + tabIndex lets keyboard users Enter/Space
               * to trigger. */
              <div
                role="button"
                tabIndex={0}
                className="flex cursor-pointer items-center gap-1 rounded-l-md px-2 py-0.5 hover:bg-zinc-100 dark:hover:bg-zinc-700 focus:outline-none focus:ring-2 focus:ring-[var(--color-accent)]/40"
                title={item.absPath}
                onClick={handleOpen}
                onKeyDown={(e) => {
                  if (e.key === "Enter" || e.key === " ") {
                    e.preventDefault();
                    handleOpen();
                  }
                }}
              >
                {item.type === "selection" ? (
                  <Hash className="h-3 w-3 shrink-0 text-[var(--color-accent)]" />
                ) : (
                  <FileText className="h-3 w-3 shrink-0 text-zinc-400" />
                )}
                <span className="max-w-[200px] truncate">{item.name}</span>
                {item.type === "selection" && item.startLine != null && item.endLine != null && (
                  <span className="text-zinc-400 dark:text-zinc-500">
                    {item.startLine}-{item.endLine}
                  </span>
                )}
              </div>
            ) : (
              /* Non-openable (folder): plain body, no click affordance. */
              <div className="flex items-center gap-1 rounded-l-md px-2 py-0.5">
                <Folder className="h-3 w-3 shrink-0 text-amber-500" />
                <span className="max-w-[200px] truncate">{item.name}</span>
              </div>
            )}
            {/* Real <button> for X — outside the clickable div, so we don't
                get a button-in-button DOM structure. */}
            <button
              type="button"
              className="mr-0.5 rounded p-0.5 text-zinc-400 hover:bg-zinc-200 hover:text-zinc-600 dark:hover:bg-zinc-700 dark:hover:text-zinc-300"
              onClick={handleRemove}
              aria-label={`Remove ${item.name}`}
            >
              <X className="h-3 w-3" />
            </button>
          </div>
        );
      })}
    </div>
  );
}