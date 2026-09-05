/**
 * ProjectBoard — 右侧看板容器（T2-3 + T2-7）。
 *
 * 对齐 UX 设计 §3.1/§3.2：
 * - 组合 ProjectHeader（标题/描述/统计/新建/菜单）+ KanbanBoard（4 列）
 * - 三态：未选中项目 → 空提示；加载中 → 骨架；错误 → 重试
 * - 新建任务：通过 onNewTask 上抛给 ProjectsView（由 TaskEditDialog 处理）
 */

import { useTranslation } from "../../i18n/useTranslation";
import { ProjectHeader } from "./ProjectHeader";
import { KanbanBoard } from "./KanbanBoard";
import type { PmProject } from "../../lib/pm-types";

interface ProjectBoardProps {
  project: PmProject | null;
  loading?: boolean;
  error?: string | null;
  onRetry?: () => void;
  onOpenTask?: (taskId: string) => void;
  /** 新建任务（打开 TaskEditDialog create 模式） */
  onNewTask?: () => void;
}

export function ProjectBoard({
  project,
  loading,
  error,
  onRetry,
  onOpenTask,
  onNewTask,
}: ProjectBoardProps) {
  const { t } = useTranslation();
  const openTask = onOpenTask ?? (() => {});
  const newTask = onNewTask ?? (() => {});

  // 未选中项目 → 空提示
  if (!project) {
    return (
      <main className="flex min-w-0 flex-1 items-center justify-center bg-chat-area">
        <div className="text-center">
          <div className="text-3xl">📋</div>
          <p className="mt-2 text-xs text-zinc-400 dark:text-zinc-500">
            {t("pm.selectProjectHint")}
          </p>
        </div>
      </main>
    );
  }

  // 加载骨架
  if (loading) {
    return (
      <main className="flex min-w-0 flex-1 flex-col">
        <div className="shrink-0 border-b border-zinc-200 px-4 py-3 dark:border-zinc-700">
          <div className="h-5 w-40 animate-pulse rounded bg-zinc-100 dark:bg-zinc-800" />
          <div className="mt-2 h-3 w-64 animate-pulse rounded bg-zinc-100 dark:bg-zinc-800" />
        </div>
        <div className="flex flex-1 gap-3 p-3">
          {[0, 1, 2, 3].map((i) => (
            <div key={i} className="flex-1 animate-pulse rounded-md bg-zinc-100 dark:bg-zinc-800" />
          ))}
        </div>
      </main>
    );
  }

  return (
    <main className="flex min-w-0 flex-1 flex-col bg-chat-area">
      <ProjectHeader project={project} onNewTask={newTask} />
      {error && (
        <div className="flex shrink-0 items-center justify-between gap-2 border-b border-zinc-200 bg-amber-50 px-4 py-2 text-xs text-amber-700 dark:border-zinc-700 dark:bg-amber-950/40 dark:text-amber-300">
          <span className="min-w-0 truncate">{error}</span>
          {onRetry && (
            <button
              type="button"
              onClick={onRetry}
              className="shrink-0 rounded-md px-2 py-0.5 text-[11px] font-medium text-amber-800 hover:bg-amber-100 dark:text-amber-200 dark:hover:bg-amber-900"
            >
              {t("common.retry")}
            </button>
          )}
        </div>
      )}
      <div className="min-h-0 flex-1">
        <KanbanBoard projectId={project.id} onOpenTask={openTask} />
      </div>
    </main>
  );
}
