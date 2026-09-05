/**
 * ProjectHeader — 项目头部（T2-4）。
 *
 * 对齐 UX 设计 §3.2：
 * - 标题 inline-edit（点击 → input，blur/Enter 保存）
 * - 描述单行展示，hover 编辑（点击展开为 textarea）
 * - 统计（总任务 / 进行中 / 待审核 / 已完成）
 * - [+ 新建任务] 按钮
 * - [⋯] 菜单（编辑项目、删除项目——含级联语义确认）
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { usePmProjectStore } from "../../stores/pm/projectStore";
import { usePmBoardStore } from "../../stores/pm/boardStore";
import { usePmHealthStore } from "../../stores/pm/healthStore";
import { ConfirmDialog } from "../../components/common/ConfirmDialog";
import { showToast } from "../../components/common/ToastProvider";
import { useTranslation } from "../../i18n/useTranslation";
import type { PmProject } from "../../lib/pm-types";

interface ProjectHeaderProps {
  project: PmProject;
  onNewTask: () => void;
}

export function ProjectHeader({ project, onNewTask }: ProjectHeaderProps) {
  const { t } = useTranslation();
  const updateProjectMeta = usePmProjectStore((s) => s.updateProjectMeta);
  const deleteProject = usePmProjectStore((s) => s.deleteProject);
  const tasks = usePmBoardStore((s) => s.tasks);
  const healthy = usePmHealthStore((s) => s.healthy);
  const offline = healthy === false;

  const [editingTitle, setEditingTitle] = useState(false);
  const [titleDraft, setTitleDraft] = useState(project.title);
  const [editingDesc, setEditingDesc] = useState(false);
  const [descDraft, setDescDraft] = useState(project.description);
  const [menuOpen, setMenuOpen] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const titleInputRef = useRef<HTMLInputElement>(null);

  // 统计：从看板任务计算
  const total = tasks.length;
  const inProgress = tasks.filter((t) => t.status === "in_progress").length;
  const submitted = tasks.filter((t) => t.status === "submitted").length;
  const done = tasks.filter((t) => t.status === "done").length;

  useEffect(() => {
    if (editingTitle) titleInputRef.current?.focus();
  }, [editingTitle]);

  const saveTitle = useCallback(async () => {
    const trimmed = titleDraft.trim();
    setEditingTitle(false);
    if (!trimmed || trimmed === project.title) return;
    const ok = await updateProjectMeta(project.id, { title: trimmed });
    if (ok) {
      showToast({ type: "success", message: t("pm.projectUpdated") });
    }
  }, [titleDraft, project, updateProjectMeta, t]);

  const saveDesc = useCallback(async () => {
    setEditingDesc(false);
    if (descDraft === project.description) return;
    const ok = await updateProjectMeta(project.id, { description: descDraft });
    if (ok) {
      showToast({ type: "success", message: t("pm.projectUpdated") });
    }
  }, [descDraft, project, updateProjectMeta, t]);

  const handleDelete = useCallback(async () => {
    setConfirmDelete(false);
    setMenuOpen(false);
    const ok = await deleteProject(project.id);
    if (ok) {
      showToast({ type: "success", message: t("pm.projectDeleted") });
    }
  }, [project.id, deleteProject, t]);

  return (
    <header className="shrink-0 border-b border-zinc-200 px-4 py-3 dark:border-zinc-700">
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0 flex-1">
          {/* 标题 inline-edit */}
          {editingTitle ? (
            <input
              ref={titleInputRef}
              value={titleDraft}
              onChange={(e) => setTitleDraft(e.target.value)}
              onBlur={saveTitle}
              onKeyDown={(e) => {
                if (e.key === "Enter") saveTitle();
                if (e.key === "Escape") {
                  setTitleDraft(project.title);
                  setEditingTitle(false);
                }
              }}
              className="w-full rounded border border-zinc-200 bg-modal-surface px-2 py-0.5 text-base font-semibold text-zinc-800 outline-none focus:border-[var(--color-accent)] dark:border-zinc-600 dark:bg-zinc-900 dark:text-zinc-100"
              aria-label={t("pm.projectTitleEdit")}
            />
          ) : (
            <h2
              className="cursor-text text-base font-semibold text-zinc-800 hover:text-zinc-600 dark:text-zinc-100 dark:hover:text-zinc-300"
              onClick={() => {
                setTitleDraft(project.title);
                setEditingTitle(true);
              }}
              title={t("pm.clickToEdit")}
              tabIndex={0}
              role="button"
              onKeyDown={(e) => {
                if (e.key === "Enter") {
                  setTitleDraft(project.title);
                  setEditingTitle(true);
                }
              }}
            >
              {project.title}
            </h2>
          )}

          {/* 描述 hover 编辑 */}
          {editingDesc ? (
            <textarea
              value={descDraft}
              onChange={(e) => setDescDraft(e.target.value)}
              onBlur={saveDesc}
              onKeyDown={(e) => {
                if (e.key === "Escape") {
                  setDescDraft(project.description);
                  setEditingDesc(false);
                }
              }}
              className="mt-1 w-full resize-y rounded border border-zinc-200 bg-modal-surface px-2 py-1 text-xs text-zinc-500 outline-none focus:border-[var(--color-accent)] dark:border-zinc-600 dark:bg-zinc-900 dark:text-zinc-300"
              rows={2}
              aria-label={t("pm.projectDescEdit")}
            />
          ) : (
            <p
              className="mt-1 line-clamp-1 cursor-text text-xs text-zinc-500 hover:text-zinc-400 dark:text-zinc-400 dark:hover:text-zinc-300"
              onClick={() => {
                setDescDraft(project.description);
                setEditingDesc(true);
              }}
              title={t("pm.clickToEdit")}
            >
              {project.description || <span className="italic opacity-60">{t("pm.noDescription")}</span>}
            </p>
          )}

          {/* 统计 */}
          <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px] text-zinc-500 dark:text-zinc-400">
            <span>
              {t("pm.statTotal")}: <strong className="tabular-nums">{total}</strong>
            </span>
            <span>
              {t("pm.board.inProgress")}: <strong className="tabular-nums">{inProgress}</strong>
            </span>
            {submitted > 0 && (
              <span className="text-amber-600 dark:text-amber-400">
                {t("pm.board.submitted")}: <strong className="tabular-nums">{submitted}</strong>
              </span>
            )}
            <span>
              {t("pm.board.done")}: <strong className="tabular-nums">{done}</strong>
            </span>
          </div>
        </div>

        <div className="flex shrink-0 items-center gap-1">
          <button
            type="button"
            onClick={onNewTask}
            disabled={offline}
            className="rounded-md bg-zinc-800 px-2.5 py-1 text-xs font-medium text-white hover:bg-zinc-700 disabled:cursor-not-allowed disabled:opacity-40 dark:bg-zinc-700 dark:hover:bg-zinc-600"
          >
            + {t("pm.newTask")}
          </button>

          {/* ⋮ 菜单 */}
          <div className="relative">
            <button
              type="button"
              onClick={() => setMenuOpen((v) => !v)}
              className="rounded-md px-1.5 py-1 text-xs text-zinc-500 hover:bg-zinc-200 hover:text-zinc-700 dark:text-zinc-400 dark:hover:bg-zinc-700 dark:hover:text-zinc-200"
              aria-label={t("pm.projectMenu")}
              aria-expanded={menuOpen}
            >
              ⋮
            </button>
            {menuOpen && (
              <>
                <div className="fixed inset-0 z-30" onClick={() => setMenuOpen(false)} />
                <div
                  className="absolute right-0 top-full z-40 mt-1 w-44 overflow-hidden rounded-md border border-zinc-200 bg-modal-surface py-1 shadow-lg dark:border-zinc-700"
                  role="menu"
                >
                  <button
                    type="button"
                    role="menuitem"
                    className="block w-full px-3 py-1.5 text-left text-xs text-red-600 hover:bg-red-50 dark:text-red-400 dark:hover:bg-red-950/40"
                    onClick={() => {
                      setMenuOpen(false);
                      setConfirmDelete(true);
                    }}
                  >
                    {t("common.deleteProject")}
                  </button>
                </div>
              </>
            )}
          </div>
        </div>
      </div>

      {/* 删除项目确认 — 级联语义提示 */}
      <ConfirmDialog
        open={confirmDelete}
        title={t("pm.deleteProjectTitle")}
        message={`${t("pm.deleteProjectCascadeDesc")} "${project.title}"?`}
        confirmLabel={t("common.delete")}
        destructive
        onConfirm={handleDelete}
        onCancel={() => setConfirmDelete(false)}
      />
    </header>
  );
}
