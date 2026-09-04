/**
 * ProjectsView — 顶级项目管理视图（T2-1 布局壳）。
 *
 * 对齐 UX 设计 §2.1/§3.1：左侧 ProjectSidebar（240px）+ 右侧 ProjectBoard。
 * 职责：
 * - 进入视图时加载项目列表 + 健康检查
 * - 组合 Sidebar / Board / TaskDetailDrawer / TaskEditDialog
 * - 三态：加载骨架 / 空状态（创建第一个项目）/ 错误 banner + 重试
 *
 * 注意：Desktop 无 react-router，路由由 AppLayout 的 NavView state 驱动；
 * 本项目内选中项由 usePmProjectStore.selected 维护。
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { usePmProjectStore } from "../stores/pm/projectStore";
import { usePmBoardStore } from "../stores/pm/boardStore";
import { usePmTaskDetailStore } from "../stores/pm/taskDetailStore";
import { usePmHealthStore } from "../stores/pm/healthStore";
import { ProjectSidebar } from "./pm/ProjectSidebar";
import { ProjectBoard } from "./pm/ProjectBoard";
import { TaskDetailDrawer } from "./pm/TaskDetailDrawer";
import { TaskEditDialog } from "./pm/TaskEditDialog";
import { ServiceOfflineBanner } from "./pm/ServiceOfflineBanner";
import { useTranslation } from "../i18n/useTranslation";
import type { PmTaskResponse } from "../lib/pm-types";

/** 编辑对话框状态：创建（项目级/子任务级）或 编辑（某任务） */
type EditDialogState =
  | { mode: "create"; projectId: string; parentId: string | null }
  | { mode: "edit"; task: PmTaskResponse }
  | null;

export function ProjectsView() {
  const { t } = useTranslation();
  const [editDialog, setEditDialog] = useState<EditDialogState>(null);
  const projects = usePmProjectStore((s) => s.projects);
  const selected = usePmProjectStore((s) => s.selected);
  const loadingProjects = usePmProjectStore((s) => s.loading);
  const loadProjects = usePmProjectStore((s) => s.loadProjects);

  const boardLoading = usePmBoardStore((s) => s.loading);
  const boardError = usePmBoardStore((s) => s.error);
  const loadTasks = usePmBoardStore((s) => s.loadTasks);
  const clearBoard = usePmBoardStore((s) => s.clear);

  const detailTaskId = usePmTaskDetailStore((s) => s.taskId);
  const openTask = usePmTaskDetailStore((s) => s.openTask);
  const clearDetail = usePmTaskDetailStore((s) => s.clear);

  // T2-14：记录打开 Drawer 的触发元素，关闭后恢复焦点
  const lastTriggerRef = useRef<HTMLElement | null>(null);
  const handleOpenTask = useCallback(
    (taskId: string) => {
      lastTriggerRef.current = document.activeElement as HTMLElement | null;
      void openTask(taskId);
    },
    [openTask],
  );
  const handleCloseDetail = useCallback(() => {
    clearDetail();
    // 等 Drawer 卸载后再恢复焦点；触发元素可能已卸载（如子树内点击），仅当仍在文档中才聚焦
    requestAnimationFrame(() => {
      const el = lastTriggerRef.current;
      if (el && el.isConnected) el.focus();
    });
  }, [clearDetail]);

  const checkHealth = usePmHealthStore((s) => s.check);

  // 进入视图：加载项目列表 + 健康检查
  useEffect(() => {
    loadProjects();
    checkHealth();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // 健康轮询：启动 + 每 30s
  useEffect(() => {
    const timer = setInterval(() => checkHealth(), 30_000);
    return () => clearInterval(timer);
  }, [checkHealth]);

  // 选中项目变化 → 加载该看板任务（清空 Drawer）
  useEffect(() => {
    if (selected) {
      loadTasks(selected.id, { silent: true });
    } else {
      clearBoard();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selected?.id]);

  const handleRetry = useCallback(() => {
    loadProjects({ silent: false });
    if (selected) loadTasks(selected.id, { silent: false });
    checkHealth();
  }, [loadProjects, loadTasks, selected, checkHealth]);

  // 三态：加载骨架
  if (loadingProjects && projects.length === 0) {
    return (
      <div className="flex h-full w-full overflow-hidden rounded-xl bg-chat-area">
        <div className="w-60 shrink-0 animate-pulse space-y-2 border-r border-zinc-200 p-3 dark:border-zinc-700">
          {[0, 1, 2, 3, 4].map((i) => (
            <div key={i} className="h-9 rounded-md bg-zinc-100 dark:bg-zinc-800" />
          ))}
        </div>
        <div className="flex-1 p-6">
          <div className="h-8 w-48 animate-pulse rounded-md bg-zinc-100 dark:bg-zinc-800" />
          <div className="mt-6 flex gap-4">
            {[0, 1, 2, 3].map((i) => (
              <div key={i} className="h-64 flex-1 animate-pulse rounded-md bg-zinc-100 dark:bg-zinc-800" />
            ))}
          </div>
        </div>
      </div>
    );
  }

  // 空状态：还没有项目 → 鼓励创建
  // 同时渲染 <ProjectSidebar />，让左侧 "+" 按钮可见、可点（健康时）。
  // 中央"创建项目"按钮直接调 usePmProjectStore.openCreate() 触发同一份对话框状态，
  // 不再依赖脆弱的 `document.getElementById(...)?.click()` DOM 反查。
  if (projects.length === 0 && !loadingProjects) {
    return (
      <div className="flex h-full w-full flex-col overflow-hidden rounded-xl bg-chat-area">
        <ServiceOfflineBanner />
        <div className="flex min-h-0 flex-1">
          <ProjectSidebar />
          <main className="flex min-w-0 flex-1 items-center justify-center">
            <div className="flex flex-col items-center justify-center gap-4 p-8">
              <div className="flex h-16 w-16 items-center justify-center rounded-full bg-zinc-100 text-3xl dark:bg-zinc-800">
                📋
              </div>
              <h2 className="text-base font-semibold text-zinc-700 dark:text-zinc-200">
                {t("pm.emptyTitle")}
              </h2>
              <p className="max-w-sm text-center text-xs text-zinc-500 dark:text-zinc-400">
                {t("pm.emptyDesc")}
              </p>
              <button
                type="button"
                className="rounded-md bg-zinc-800 px-4 py-1.5 text-xs font-medium text-white hover:bg-zinc-700 disabled:opacity-50 dark:bg-zinc-700 dark:hover:bg-zinc-600"
                onClick={() => usePmProjectStore.getState().openCreate()}
              >
                {t("pm.newProject")}
              </button>
            </div>
          </main>
        </div>
      </div>
    );
  }

  return (
    <div className="flex h-full w-full flex-col overflow-hidden rounded-xl bg-chat-area">
      <ServiceOfflineBanner />
      <div className="flex min-h-0 flex-1">
        {/* 左侧项目列表 — 240px 固定宽 */}
        <ProjectSidebar />
        {/* 右侧看板 — flex-1 */}
        <ProjectBoard
          project={selected}
          loading={boardLoading}
          error={boardError}
          onRetry={handleRetry}
          onOpenTask={handleOpenTask}
          onNewTask={() => {
            if (selected) setEditDialog({ mode: "create", projectId: selected.id, parentId: null });
          }}
        />
      </div>
      {/* 任务详情抽屉（含编辑/添加子任务） */}
      {detailTaskId && (
        <TaskDetailDrawer
          taskId={detailTaskId}
          onClose={handleCloseDetail}
          onEdit={(task) => setEditDialog({ mode: "edit", task })}
          onAddSubtask={(parentId) => {
            if (selected) setEditDialog({ mode: "create", projectId: selected.id, parentId });
          }}
        />
      )}
      {/* 创建/编辑任务对话框 */}
      {editDialog && editDialog.mode === "create" && (
        <TaskEditDialog
          mode="create"
          projectId={editDialog.projectId}
          defaultParentId={editDialog.parentId}
          onClose={() => setEditDialog(null)}
          onSaved={() => {
            if (detailTaskId) usePmTaskDetailStore.getState().refresh();
          }}
        />
      )}
      {editDialog && editDialog.mode === "edit" && (
        <TaskEditDialog
          mode="edit"
          projectId={editDialog.task.project_id}
          initial={editDialog.task}
          onClose={() => setEditDialog(null)}
          onSaved={() => {
            usePmBoardStore.getState().reload();
            if (detailTaskId) usePmTaskDetailStore.getState().refresh();
          }}
        />
      )}
    </div>
  );
}

// 辅助：避免 useMemo 未使用告警（boardLoading 在骨架/空态外用于 ProjectBoard）
export const _projectsViewHooks = { useMemo };
