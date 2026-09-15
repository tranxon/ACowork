/**
 * GitStatusPanel — ADR-078 decision 6. Flat (non-tree) list of uncommitted
 * changes below GitStatusBar. Row styling mirrors the file-tree rows
 * (FileTreeNode.tsx). Right-click menu: Show Diff / Show Log / Open in
 * editor. Deleted rows redirect to Show Diff (decision 6).
 */

import { useMemo } from "react";
import {
  AlertCircle,
  ArrowRightLeft,
  FileMinus,
  FilePlus,
  GitMerge,
  Pencil,
} from "lucide-react";
import {
  type GitChangeDto,
  type GitDiffResponse,
  type GitLogResponse,
  gitGroupKey,
  useGitStore,
} from "../../../stores/gitStore";
import { useFileEditorStore } from "../../../stores/fileEditorStore";
import { useTranslation } from "../../../i18n/useTranslation";
import { useContextMenu, ContextMenu } from "../../common/ContextMenu";
import { cn } from "../../../lib/utils";

interface GitStatusPanelProps {
  agentId: string;
  workspaceId: string;
}

/** Loose extension → Monaco language mapping for virtual diff/log tabs. */
function languageForPath(path: string): string {
  const ext = path.split(".").pop()?.toLowerCase() ?? "";
  const map: Record<string, string> = {
    ts: "typescript",
    tsx: "typescript",
    js: "javascript",
    jsx: "javascript",
    json: "json",
    md: "markdown",
    rs: "rust",
    py: "python",
    go: "go",
    java: "java",
    c: "c",
    h: "c",
    cpp: "cpp",
    hpp: "cpp",
    css: "css",
    scss: "scss",
    less: "less",
    html: "html",
    htm: "html",
    xml: "xml",
    yaml: "yaml",
    yml: "yaml",
    sh: "shell",
    bash: "shell",
    zsh: "shell",
    toml: "ini",
    ini: "ini",
    sql: "sql",
  };
  return map[ext] ?? "plaintext";
}

function statusMeta(c: GitChangeDto): { icon: React.ReactNode; color: string; labelKey: string } {
  // Unmerged / conflicted paths take priority — never rendered as "clean"
  // (ADR-078 invariant 5; runtime maps porcelain `UU`/`AU`/`AA`/... to
  // `conflicted` on both columns).
  if (c.index === "conflicted" || c.worktree === "conflicted") {
    return {
      icon: <GitMerge size={13} />,
      color: "text-red-500 dark:text-red-400",
      labelKey: "gitStatus.conflicted",
    };
  }
  if (c.index === "renamed" || c.index === "added") {
    return {
      icon: c.index === "renamed" ? <ArrowRightLeft size={13} /> : <FilePlus size={13} />,
      color: c.staged ? "text-emerald-600 dark:text-emerald-400" : "text-zinc-500 dark:text-zinc-400",
      labelKey: c.index === "renamed" ? "gitStatus.renamed" : "gitStatus.added",
    };
  }
  if (c.index === "deleted" || c.worktree === "deleted") {
    return {
      icon: <FileMinus size={13} />,
      color: "text-red-500 dark:text-red-400",
      labelKey: "gitStatus.deleted",
    };
  }
  if (c.worktree === "untracked") {
    return {
      icon: <FilePlus size={13} />,
      color: "text-sky-500 dark:text-sky-400",
      labelKey: "gitStatus.untracked",
    };
  }
  return {
    icon: <Pencil size={13} />,
    color: "text-amber-500 dark:text-amber-400",
    labelKey: "gitStatus.modified",
  };
}

export function GitStatusPanel({ agentId, workspaceId }: GitStatusPanelProps) {
  const { t } = useTranslation();
  const entry = useGitStore((s) => s.status[gitGroupKey(agentId, workspaceId)]);
  const menu = useContextMenu<GitChangeDto>();

  const data = entry?.data;
  const loading = entry?.loading ?? false;
  const error = entry?.error ?? null;
  const changes = data?.changes ?? [];

  const openDiff = useMemo(
    () => async (c: GitChangeDto) => {
      const editor = useFileEditorStore.getState();
      try {
        const diff: GitDiffResponse = await useGitStore
          .getState()
          .fetchDiff(agentId, workspaceId, c.path, 0);
        editor.openVirtualFile({
          agentId,
          workspaceId,
          kind: "diff",
          relPath: c.path,
          content: diff.modified,
          original: diff.original,
          gitDiffKind: diff.kind,
          language: languageForPath(c.path),
        });
      } catch (e) {
        console.error("[GitStatusPanel] fetchDiff failed:", e);
      }
    },
    [agentId, workspaceId],
  );

  const openLog = useMemo(
    () => async (c: GitChangeDto) => {
      const editor = useFileEditorStore.getState();
      try {
        const log: GitLogResponse = await useGitStore
          .getState()
          .fetchLog(agentId, workspaceId, c.path, 50);
        const text = log.commits
          .map(
            (cm) =>
              `${cm.shortHash}  ${cm.author}  ${cm.date}\n    ${cm.subject}`,
          )
          .join("\n\n");
        editor.openVirtualFile({
          agentId,
          workspaceId,
          kind: "log",
          relPath: c.path,
          content: text || t("gitStatus.noCommits"),
          language: "plaintext",
        });
      } catch (e) {
        console.error("[GitStatusPanel] fetchLog failed:", e);
      }
    },
    [agentId, workspaceId, t],
  );

  const menuItems = useMemo(
    () => [
      {
        key: "diff",
        label: t("gitStatus.showDiff"),
        onClick: ({ payload }: { payload: GitChangeDto | undefined }) => {
          if (payload) void openDiff(payload);
        },
      },
      {
        key: "log",
        label: t("gitStatus.showLog"),
        onClick: ({ payload }: { payload: GitChangeDto | undefined }) => {
          if (payload) void openLog(payload);
        },
      },
      {
        key: "open",
        label: t("gitStatus.openInEditor"),
        onClick: ({ payload }: { payload: GitChangeDto | undefined }) => {
          if (payload && payload.worktree !== "deleted") {
            void useFileEditorStore
              .getState()
              .openFile(agentId, workspaceId, payload.path);
          } else if (payload) {
            // Deleted rows redirect to Show Diff (ADR-078 decision 6).
            void openDiff(payload);
          }
        },
      },
    ],
    [t, openDiff, openLog, agentId, workspaceId],
  );

  const body = (() => {
    if (loading && !data) {
      return (
        <div className="flex h-full items-center justify-center text-xs text-zinc-400">
          {t("gitStatus.loading")}
        </div>
      );
    }
    if (error && !data) {
      return (
        <div className="flex h-full items-center justify-center gap-1.5 text-xs text-red-500">
          <AlertCircle size={13} /> {error}
        </div>
      );
    }
    if (!data) return null;
    if (!data.isRepo) {
      return (
        <div className="flex h-full items-center justify-center text-xs text-zinc-400">
          {data.error === "git_unavailable"
            ? t("gitStatus.gitUnavailable")
            : t("gitStatus.notRepo")}
        </div>
      );
    }
    if (changes.length === 0) {
      return (
        <div className="flex h-full items-center justify-center text-xs text-zinc-400">
          {t("gitStatus.clean")}
        </div>
      );
    }
    return (
      <ul className="max-h-[200px] overflow-y-auto py-0.5">
        {changes.map((c) => {
          const meta = statusMeta(c);
          return (
            <li
              key={c.path}
              onContextMenu={(e) => menu.openAt(e, c)}
              className={cn(
                "file-tree-row flex cursor-pointer items-center gap-1.5 py-[0.2em] pr-3 pl-4 text-xs select-none",
                "hover:bg-zinc-100 dark:hover:bg-zinc-800",
              )}
              onClick={() => {
                if (c.worktree === "deleted") void openDiff(c);
                else
                  void useFileEditorStore
                    .getState()
                    .openFile(agentId, workspaceId, c.path);
              }}
              data-testid="git-status-row"
            >
              <span className={cn("shrink-0", meta.color)} title={t(meta.labelKey)}>
                {meta.icon}
              </span>
              <span className="flex-1 truncate text-zinc-700 dark:text-zinc-300">
                {c.path}
              </span>
              {c.oldPath && (
                <span className="truncate text-[10px] text-zinc-400 dark:text-zinc-500">
                  ← {c.oldPath}
                </span>
              )}
              {c.staged && (
                <span
                  className="shrink-0 rounded bg-emerald-100 px-1 text-[10px] text-emerald-700 dark:bg-emerald-900/40 dark:text-emerald-300"
                  title={t("gitStatus.staged")}
                >
                  {t("gitStatus.staged")}
                </span>
              )}
            </li>
          );
        })}
      </ul>
    );
  })();

  return (
    <div
      className="shrink-0 border-b border-right-panel-border bg-page-bg"
      data-testid="git-status-panel"
    >
      {body}
      <ContextMenu
        isOpen={menu.isOpen}
        menuProps={menu.menuProps}
        items={menuItems}
        payload={menu.payload}
        selectionAtOpen={menu.selectionAtOpen}
        onClose={menu.close}
        compact
      />
      {/* manual refresh affordance is on the bar; keep the panel focused */}
      <span className="sr-only">{loading ? t("gitStatus.loading") : ""}</span>
    </div>
  );
}
