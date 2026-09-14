/**
 * GitStatusBar — ADR-078 decision 6. Collapsible version-control strip at
 * the bottom of the FileEditorPanel. Visual spec follows NodeGroupHeader
 * (AgentList.tsx): h-6, 10px uppercase tracking-wide, zinc-400/500,
 * border-y, hover tint, ChevronRight rotation.
 */

import { useEffect } from "react";
import { ChevronRight, GitBranch, Loader2, RefreshCw } from "lucide-react";
import { gitGroupKey, useGitStore } from "../../../stores/gitStore";
import { useTranslation } from "../../../i18n/useTranslation";
import { cn } from "../../../lib/utils";

interface GitStatusBarProps {
  agentId: string;
  workspaceId: string;
}

export function GitStatusBar({ agentId, workspaceId }: GitStatusBarProps) {
  const { t } = useTranslation();
  const isExpanded = useGitStore((s) => s.isExpanded(agentId, workspaceId));
  const entry = useGitStore((s) => s.status[gitGroupKey(agentId, workspaceId)]);
  const setExpanded = useGitStore((s) => s.setExpanded);
  const refresh = useGitStore((s) => s.refresh);

  // ADR-078 decision 6 / invariant 6 ("subscription == visibility"): the
  // expanded (subscribed) group must match the group this bar renders.
  //   - Setup: a stale expansion from a previous agent/workspace (panel
  //     switched groups without collapsing) is cleared so the demand-driven
  //     fs-watch subscription (workspaceFsWatch deriveWatchGroups) is
  //     released instead of watching the old group in the background.
  //   - Cleanup: when this bar unmounts (right panel collapsed / switched
  //     to another tab) or the (agent, workspace) group changes, collapse
  //     this group if expanded — the panel is no longer visible.
  useEffect(() => {
    const s = useGitStore.getState();
    if (s.expandedKey && !s.isExpanded(agentId, workspaceId)) {
      s.setExpanded(agentId, workspaceId, false);
    }
    return () => {
      const cur = useGitStore.getState();
      if (cur.isExpanded(agentId, workspaceId)) {
        cur.setExpanded(agentId, workspaceId, false);
      }
    };
  }, [agentId, workspaceId]);

  const data = entry?.data;
  const loading = entry?.loading ?? false;
  const isRepo = data?.isRepo ?? true; // optimistically interactive pre-load
  const branch = data?.branch;
  const changes = data?.changes.length ?? 0;

  const title = !data
    ? t("gitStatusBar.title")
    : !isRepo
      ? t("gitStatusBar.notRepo")
      : branch
        ? `${branch} · ${changes} ${t("gitStatusBar.changes")}`
        : `${changes} ${t("gitStatusBar.changes")}`;

  return (
    <button
      type="button"
      onClick={() => setExpanded(agentId, workspaceId, !isExpanded)}
      aria-expanded={isExpanded}
      aria-label={title}
      title={title}
      data-testid="git-status-bar"
      className={cn(
        // h-6 — mirrors NodeGroupHeader (AgentList.tsx L847).
        // Text color aligned with WorkspaceSelector's toolbarButton (lib/ui-styles.ts L29)
        // so the bottom-of-panel strip reads as part of the same toolbar family.
        "flex h-6 w-full shrink-0 items-center gap-1.5 px-3 text-left",
        "text-[10px] font-medium uppercase tracking-wide",
        "text-zinc-500 hover:text-zinc-700 dark:hover:text-zinc-200",
        "transition-colors duration-150",
        "border-y border-nav-divider/40 dark:border-zinc-600/40",
      )}
    >
      <ChevronRight
        className={cn(
          "h-3 w-3 shrink-0 transition-transform duration-150",
          isExpanded && "rotate-90",
        )}
      />
      <GitBranch className="h-3 w-3 shrink-0" />
      <span className="flex-1 truncate">{title}</span>
      {loading ? (
        <Loader2 className="h-3 w-3 shrink-0 animate-spin" />
      ) : (
        <RefreshCw
          className="h-3 w-3 shrink-0 transition-colors hover:text-zinc-700 dark:hover:text-zinc-200"
          onClick={(e) => {
            e.stopPropagation();
            void refresh(agentId, workspaceId);
          }}
        />
      )}
    </button>
  );
}
