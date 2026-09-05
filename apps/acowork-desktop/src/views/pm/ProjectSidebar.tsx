/**
 * ProjectSidebar — 左侧项目列表（T2-2）。
 *
 * 对齐 UX 设计 §3.1：
 * - 项目列表 + 选中高亮
 * - 任务计数徽章（待审核数字色高亮）
 * - 新建项目（对话框）/ 删除项目（ConfirmDialog 二次确认）
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { usePmProjectStore } from "../../stores/pm/projectStore";
import { usePmHealthStore } from "../../stores/pm/healthStore";
import { ConfirmDialog } from "../../components/common/ConfirmDialog";
import { StyledInput } from "../../components/common/StyledInput";
import { showToast } from "../../components/common/ToastProvider";
import { cn } from "../../lib/utils";
import { useTranslation } from "../../i18n/useTranslation";
import type { PmProject } from "../../lib/pm-types";

export function ProjectSidebar() {
  const { t } = useTranslation();
  const projects = usePmProjectStore((s) => s.projects);
  const selected = usePmProjectStore((s) => s.selected);
  const counts = usePmProjectStore((s) => s.counts);
  const selectProject = usePmProjectStore((s) => s.selectProject);
  const createProject = usePmProjectStore((s) => s.createProject);
  const deleteProject = usePmProjectStore((s) => s.deleteProject);
  const creating = usePmProjectStore((s) => s.creating);
  const openCreate = usePmProjectStore((s) => s.openCreate);
  const closeCreate = usePmProjectStore((s) => s.closeCreate);
  const healthy = usePmHealthStore((s) => s.healthy);

  const [title, setTitle] = useState("");
  const [saving, setSaving] = useState(false);
  const [deleting, setDeleting] = useState<PmProject | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  // 新建对话框打开时聚焦输入框（creating 现在是全局 store 状态）
  useEffect(() => {
    if (creating) inputRef.current?.focus();
  }, [creating]);

  const handleCreate = useCallback(async () => {
    const trimmed = title.trim();
    if (!trimmed) {
      showToast({ type: "warning", message: t("pm.newProjectTitleRequired") });
      return;
    }
    setSaving(true);
    const project = await createProject(trimmed);
    setSaving(false);
    if (project) {
      setTitle("");
      closeCreate();
      showToast({ type: "success", message: t("pm.projectCreated") });
    }
  }, [title, createProject, closeCreate, t]);

  const handleDelete = useCallback(async () => {
    if (!deleting) return;
    const ok = await deleteProject(deleting.id);
    setDeleting(null);
    if (ok) {
      showToast({ type: "success", message: t("pm.projectDeleted") });
    }
  }, [deleting, deleteProject, t]);

  return (
    <aside className="flex w-60 shrink-0 flex-col border-r border-zinc-200 dark:border-zinc-700">
      {/* 标题 + 新建按钮 */}
      <div className="flex items-center justify-between px-3 py-2">
        <h2 className="text-xs font-semibold uppercase tracking-wide text-zinc-500 dark:text-zinc-400">
          {t("pm.projects")}
        </h2>
        <button
          type="button"
          onClick={() => openCreate()}
          disabled={healthy === false}
          className="rounded-md px-1.5 py-0.5 text-xs text-zinc-500 hover:bg-zinc-200 hover:text-zinc-700 disabled:cursor-not-allowed disabled:opacity-40 dark:text-zinc-400 dark:hover:bg-zinc-700 dark:hover:text-zinc-200"
          aria-label={t("pm.newProject")}
          title={t("pm.newProject")}
        >
          +
        </button>
      </div>

      {/* 项目列表 */}
      <nav
        className="min-h-0 flex-1 space-y-0.5 overflow-y-auto px-2 pb-2"
        aria-label={t("pm.projectListAria")}
      >
        {projects.map((p) => {
          const active = selected?.id === p.id;
          const c = counts[p.id] ?? { total: 0, submitted: 0 };
          return (
            <button
              key={p.id}
              type="button"
              onClick={() => selectProject(p.id)}
              className={cn(
                "group flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-xs transition-colors",
                active
                  ? "bg-zinc-200/80 text-zinc-900 dark:bg-zinc-700/70 dark:text-zinc-100"
                  : "text-zinc-600 hover:bg-zinc-100 dark:text-zinc-300 dark:hover:bg-zinc-800",
              )}
              aria-current={active ? "page" : undefined}
            >
              <span className="min-w-0 flex-1 truncate">{p.title}</span>
              {/* 计数徽章：待审核数字色高亮 */}
              {c.total > 0 && (
                <span className="flex shrink-0 items-center gap-1">
                  {c.submitted > 0 && (
                    <span className="rounded-full bg-amber-100 px-1.5 text-[10px] font-medium text-amber-700 dark:bg-amber-900/50 dark:text-amber-300">
                      {c.submitted}
                    </span>
                  )}
                  <span className="rounded-full bg-zinc-100 px-1.5 text-[10px] text-zinc-500 group-hover:bg-zinc-200 dark:bg-zinc-800 dark:text-zinc-400">
                    {c.total}
                  </span>
                </span>
              )}
            </button>
          );
        })}
      </nav>

      {/* 离线时禁用写操作 */}
      {healthy === false && (
        <div className="border-t border-zinc-200 px-3 py-2 text-[10px] text-zinc-400 dark:border-zinc-700 dark:text-zinc-500">
          {t("pm.offlineReadonlyHint")}
        </div>
      )}

      {/* 新建项目对话框 */}
      {creating && (
        <div className="fixed inset-0 z-50 flex items-center justify-center">
          <div
            className="absolute inset-0 bg-modal-overlay"
            onClick={() => closeCreate()}
          />
          <div
            className="relative z-10 w-full max-w-sm rounded-md border border-zinc-200 bg-modal-surface p-5 shadow-xl dark:border-zinc-700"
            role="dialog"
            aria-modal="true"
            aria-labelledby="pm-new-project-title"
          >
            <h3 id="pm-new-project-title" className="text-sm font-semibold">
              {t("pm.newProject")}
            </h3>
            <div className="mt-3">
              <StyledInput
                ref={inputRef}
                value={title}
                onChange={(e) => setTitle(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") handleCreate();
                  if (e.key === "Escape") closeCreate();
                }}
                placeholder={t("pm.newProjectPlaceholder")}
                disabled={saving}
              />
            </div>
            <div className="mt-4 flex justify-end gap-2">
              <button
                type="button"
                onClick={() => closeCreate()}
                className="rounded-md px-3 py-1.5 text-xs font-medium text-zinc-600 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-700"
                disabled={saving}
              >
                {t("common.cancel")}
              </button>
              <button
                type="button"
                onClick={handleCreate}
                className="rounded-md bg-zinc-800 px-3 py-1.5 text-xs font-medium text-white hover:bg-zinc-700 disabled:opacity-50 dark:bg-zinc-700 dark:hover:bg-zinc-600"
                disabled={saving}
              >
                {saving ? t("common.saving") : t("common.create")}
              </button>
            </div>
          </div>
        </div>
      )}

      {/* 删除项目确认 */}
      <ConfirmDialog
        open={!!deleting}
        title={t("pm.deleteProjectTitle")}
        message={`${t("pm.deleteProjectDesc")} "${deleting?.title ?? ""}"?`}
        confirmLabel={t("common.delete")}
        destructive
        onConfirm={handleDelete}
        onCancel={() => setDeleting(null)}
      />
    </aside>
  );
}
