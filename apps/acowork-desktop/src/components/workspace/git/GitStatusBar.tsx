/**
 * GitStatusBar — ADR-078 decision 6. Collapsible version-control strip at
 * the bottom of the FileEditorPanel. Visual spec follows NodeGroupHeader
 * (AgentList.tsx): h-6, 10px uppercase tracking-wide, zinc-400/500,
 * border-y, hover tint, ChevronRight rotation.
 *
 * Right-side controls: `History` (clock icon) opens a CommitPicker
 * dropdown letting the user view files from any commit on the
 * repository's history, with `Local Working Tree` pinned at the top;
 * `Refresh` reloads the CURRENTLY-VIEWED view (working tree or the
 * selected commit's file list).
 */

import { useEffect, useState } from "react";
import { ChevronRight, Clock, GitBranch, Loader2, RefreshCw } from "lucide-react";
import { gitGroupKey, gitViewKey, useGitStore } from "../../../stores/gitStore";
import { useTranslation } from "../../../i18n/useTranslation";
import { cn } from "../../../lib/utils";
import { CommitPicker } from "../../editor/CommitPicker";
import { Tooltip } from "../../common/Tooltip";

interface GitStatusBarProps {
  agentId: string;
  workspaceId: string;
}

export function GitStatusBar({ agentId, workspaceId }: GitStatusBarProps) {
  const { t } = useTranslation();
  const isExpanded = useGitStore((s) => s.isExpanded(agentId, workspaceId));
  const groupKey = gitGroupKey(agentId, workspaceId);
  // Always read the view-slot the panel is currently rendering so the
  // bar title + History button reflect the active view (working tree
  // by default, or the commit the user picked from the dropdown).
  const viewingRev = useGitStore((s) => s.viewingRev[groupKey] ?? "");
  const entry = useGitStore((s) => s.status[gitViewKey(groupKey, viewingRev)]);
  const setExpanded = useGitStore((s) => s.setExpanded);
  const refresh = useGitStore((s) => s.refresh);
  const setViewingRev = useGitStore((s) => s.setViewingRev);

  // CommitPicker anchor state. `null` means closed; toggling clicks on
  // the history button flip it. The same picker host is reused for both
  // open/close so its mount effect (fetchLog) only runs once per open.
  const [historyAnchor, setHistoryAnchor] = useState<HTMLElement | null>(null);

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

  // Auto-refresh status on mount and whenever the (agent, workspace) group
  // changes (workspace switch, agent restart while the workspace panel is
  // mounted). Mirrors FileTree.tsx's mount-time `fetchTree(agentId,
  // workspaceId, "")` effect so the banner converges without a click —
  // without this, the collapsed bar would stay on the default "Git" title
  // until the user expands it (the expand path already calls refresh, but
  // a collapsed banner should still show the current branch). Inflight
  // dedup in `fetchStatus` keeps a double-fetch from clicking the refresh
  // icon or expanding the bar from issuing a second HTTP request.
  useEffect(() => {
    void refresh(agentId, workspaceId);
  }, [agentId, workspaceId, refresh]);

  const data = entry?.data;
  const loading = entry?.loading ?? false;
  const isRepo = data?.isRepo ?? true; // optimistically interactive pre-load
  const branch = data?.branch;
  const changes = data?.changes.length ?? 0;
  // True only when the bar is showing a specific commit's file list
  // (i.e. the user picked a non-empty rev). The "files in commit X"
  // copy replaces the "branch · N changes" copy so the bar reads as
  // "you're viewing X's file list, not your worktree".
  const viewingCommit = viewingRev !== "";

  // Direction of the history dropdown. Anchor ABOVE the bar whenever
  // the popover (~288px) wouldn't fit between the bar and the
  // panel's bottom edge — i.e. the panel is collapsed (banner sits
  // flush at the panel bottom; `changes` still reads the store and
  // can be any N, so we MUST NOT key off `changes` alone), OR the
  // expanded file list is short (<6 rows, banner still near the
  // panel bottom). With a long expanded list (≥ 6 rows) the file
  // list pushes the banner up into the middle of the workspace,
// downward has room, and opening upward would cover the FileTree.
  const historyPlacement: "top" | "bottom" =
    !isExpanded || changes < 6 ? "top" : "bottom";

  const title = !data
    ? t("gitStatusBar.title")
    : !isRepo
      ? t("gitStatusBar.notRepo")
      : viewingCommit
        ? `${branch ?? viewingRev} · ${changes} ${t("gitStatusBar.changes")}`
        : branch
          ? `${branch} · ${changes} ${t("gitStatusBar.changes")}`
          : `${changes} ${t("gitStatusBar.changes")}`;

  return (
    // The wrapper div uses `display: contents` so it doesn't break the
    // bar's flex layout (the parent panel positions this element as if
    // the button were its direct child). The wrapper exists only to
    // give the CommitPicker popover a sibling outside the bar's own
    // <button> — nesting <button> inside <button> is invalid HTML
    // (React warns) AND a click on any picker item would otherwise
    // bubble up and toggle the bar's expand state.
    <div className="contents">
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
          "text-text-tertiary hover:text-zinc-700 dark:hover:text-zinc-200",
          "transition-colors duration-150",
          "border-y border-nav-divider/40 dark:border-zinc-600/40",
          // Subtle elevation when expanded. Collapsed = transparent so the
          // banner blends into `bg-right-panel` (the workspace surface);
          // expanded = `bg-zinc-50` / `dark:bg-zinc-900/40` so the banner
          // reads as the active "section" header above the file list
          // (the list itself stays on the workspace surface — no change
          // there — so the band of contrast sits on the banner only).
          // Same elevation pair as FileEditorPanel's diff header
          // (FileEditorPanel.tsx L1531), keeping strip elevations
          // consistent across panels.
          isExpanded && "bg-zinc-50 dark:bg-zinc-900/40",
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
        <div className="flex items-center gap-1 pr-1 shrink-0">
          {/* History picker button. Rendered as a <span role="button">
              because it sits inside the bar's own <button> — nested
              <button> is invalid HTML and React will warn. The class
              string + Tooltip wrap mirror SessionTabBar's session
              history button so the two toolbar strips read as a family.
              Active state (history dropdown open OR a commit is being
              viewed) shows the accent treatment, also matching the
              session history button. */}
          <div className="relative inline-flex">
            <Tooltip content={t("gitStatusBar.history")} variant="plain">
              <span
                role="button"
                tabIndex={0}
                aria-label={t("gitStatusBar.history")}
                data-testid="git-status-bar-history"
                onClick={(e) => {
                  e.stopPropagation();
                  setHistoryAnchor((cur) => (cur ? null : e.currentTarget));
                }}
                onKeyDown={(e) => {
                  if (e.key === "Enter" || e.key === " ") {
                    e.preventDefault();
                    e.stopPropagation();
                    setHistoryAnchor((cur) => (cur ? null : e.currentTarget));
                  }
                }}
                className={cn(
                  "inline-flex items-center justify-center rounded h-6 w-6 transition-colors",
                  historyAnchor || viewingCommit
                    ? "text-[var(--color-accent)] bg-zinc-200 dark:bg-zinc-700"
                    : "text-text-tertiary hover:bg-zinc-200 hover:text-zinc-600 dark:hover:bg-zinc-700 dark:hover:text-zinc-300",
                )}
              >
                <Clock className="h-3.5 w-3.5" />
              </span>
            </Tooltip>
          </div>
          <div className="relative inline-flex">
            <Tooltip content={t("gitStatusBar.refresh")} variant="plain">
              <span
                role="button"
                tabIndex={0}
                aria-label={t("gitStatusBar.refresh")}
                data-testid="git-status-bar-refresh"
                onClick={(e) => {
                  e.stopPropagation();
                  void refresh(agentId, workspaceId);
                }}
                onKeyDown={(e) => {
                  if (e.key === "Enter" || e.key === " ") {
                    e.preventDefault();
                    e.stopPropagation();
                    void refresh(agentId, workspaceId);
                  }
                }}
                className="inline-flex items-center justify-center rounded h-6 w-6 transition-colors text-text-tertiary hover:bg-zinc-200 hover:text-zinc-600 dark:hover:bg-zinc-700 dark:hover:text-zinc-300"
              >
                <RefreshCw className="h-3.5 w-3.5" />
              </span>
            </Tooltip>
          </div>
        </div>
      )}
      </button>
      {historyAnchor && (
        <CommitPicker
          // Pinned at top of the dropdown: "Local Working Tree". The
          // picker passes "" as the special working-tree ref, which
          // the store normalises to the bare group key.
          anchorEl={historyAnchor}
          currentRef={viewingRev}
          allowWorkingTree
          agentId={agentId}
          workspaceId={workspaceId}
          // Empty path → repo-wide history (the fetchLog endpoint
          // skips the path param when it's empty).
          relPath=""
          // Flip the popover above the bar when the expanded file list
          // is short (see `historyPlacement` derivation above) so the
          // dropdown doesn't clip off the workspace panel's bottom.
          placement={historyPlacement}
          onSelect={(rev) => {
            setViewingRev(agentId, workspaceId, rev);
            setHistoryAnchor(null);
          }}
          onClose={() => setHistoryAnchor(null)}
        />
      )}
    </div>
  );
}
