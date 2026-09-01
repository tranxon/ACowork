/**
 * TaskDetailDrawer — 任务详情抽屉（T2-6）。
 *
 * 对齐 UX 设计 §3.4：
 * - 右侧滑入，宽 480px，可滚动
 * - Tabs（按需显示）：概述 / 描述 / 子任务 / 依赖 / 附件
 * - 子任务树 SubtaskTree（同 Drawer 内导航到子任务）
 * - 底部：编辑 / 删除 / 状态快速流转
 * - 关闭时焦点回归原 Task Card
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { usePmTaskDetailStore } from "../../stores/pm/taskDetailStore";
import { usePmBoardStore } from "../../stores/pm/boardStore";
import { useAgentStore } from "../../stores/agentStore";
import { useTranslation } from "../../i18n/useTranslation";
import { PriorityBadge } from "./PriorityBadge";
import { TaskTypeIcon } from "./TaskTypeIcon";
import { SubtaskTree } from "./SubtaskTree";
import { ConfirmDialog } from "../../components/common/ConfirmDialog";
import { showToast } from "../../components/common/ToastProvider";
import { attachmentUrl } from "../../lib/pm-api";
import { cn } from "../../lib/utils";
import type { PmTaskResponse, TaskStatus } from "../../lib/pm-types";

interface TaskDetailDrawerProps {
  taskId: string;
  onClose: () => void;
  onEdit?: (task: PmTaskResponse) => void;
  onAddSubtask?: (parentId: string) => void;
}

type TabId = "overview" | "description" | "subtasks" | "dependencies" | "attachments";

function resolveAgentName(
  agents: Record<string, { meta?: { display_name?: string; name?: string } }>,
  id: string | null,
): string | null {
  if (!id) return null;
  const a = agents[id];
  if (!a?.meta) return id;
  return a.meta.display_name || a.meta.name || id;
}

const TASK_STATUS_I18N: Record<TaskStatus, string> = {
  pending: "pm.board.pending",
  in_progress: "pm.board.inProgress",
  submitted: "pm.board.submitted",
  done: "pm.board.done",
  rejected: "pm.board.rejected",
  cancelled: "pm.board.cancelled",
};

function statusLabel(t: (k: string) => string, status: TaskStatus): string {
  return t(TASK_STATUS_I18N[status]);
}

function formatDateTime(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}

export function TaskDetailDrawer({ taskId, onClose, onEdit, onAddSubtask }: TaskDetailDrawerProps) {
  const { t } = useTranslation();
  const detail = usePmTaskDetailStore((s) => s.detail);
  const attachments = usePmTaskDetailStore((s) => s.attachments);
  const loading = usePmTaskDetailStore((s) => s.loading);
  const uploading = usePmTaskDetailStore((s) => s.uploading);
  const error = usePmTaskDetailStore((s) => s.error);
  const openTask = usePmTaskDetailStore((s) => s.openTask);
  const refresh = usePmTaskDetailStore((s) => s.refresh);
  const uploadAttachment = usePmTaskDetailStore((s) => s.uploadAttachment);
  const deleteAttachment = usePmTaskDetailStore((s) => s.deleteAttachment);

  const agents = useAgentStore((s) => s.agents);
  const [activeTab, setActiveTab] = useState<TabId>("overview");
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [confirmDeleteAtt, setConfirmDeleteAtt] = useState<string | null>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const drawerRef = useRef<HTMLElement>(null);
  const tabRefs = useRef<Array<HTMLButtonElement | null>>([]);

  const handleTabKeyDown = (e: React.KeyboardEvent, index: number) => {
    if (e.key !== "ArrowLeft" && e.key !== "ArrowRight") return;
    e.preventDefault();
    const dir = e.key === "ArrowRight" ? 1 : -1;
    const next = (index + dir + tabs.length) % tabs.length;
    setActiveTab(tabs[next].id);
    tabRefs.current[next]?.focus();
  };

  const tabs: { id: TabId; label: string }[] = [
    { id: "overview", label: t("pm.task.overview") },
    { id: "description", label: t("pm.task.description") },
  ];
  if (detail && detail.depends_on.length > 0) tabs.push({ id: "dependencies", label: t("pm.task.dependencies") });
  tabs.push({ id: "attachments", label: t("pm.task.attachments") });
  // 子任务 Tab：有子任务或允许添加时显示
  tabs.push({ id: "subtasks", label: t("pm.task.subtasks") });

  // 打开时拉取详情 + 聚焦抽屉
  useEffect(() => {
    openTask(taskId);
    drawerRef.current?.focus();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [taskId]);

  // ESC 关闭
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [onClose]);

  const assigneeName = useMemo(
    () => resolveAgentName(agents, detail?.assignee ?? null),
    [agents, detail?.assignee],
  );
  const creatorName = useMemo(
    () => resolveAgentName(agents, detail?.created_by ?? null),
    [agents, detail?.created_by],
  );

  const removeTask = usePmBoardStore((s) => s.removeTask);

  const handleDelete = useCallback(async () => {
    setConfirmDelete(false);
    if (!detail) return;
    // 级联删除：从 api 直接调用（boardStore 无 delete，用 api + remove）
    const { deleteTask } = await import("../../lib/pm-api");
    try {
      await deleteTask(detail.id, { cascade: true });
      removeTask(detail.id);
      showToast({ type: "success", message: t("pm.task.taskDeleted") });
      onClose();
    } catch (e) {
      showToast({ type: "error", message: `删除失败: ${e instanceof Error ? e.message : e}` });
    }
  }, [detail, removeTask, onClose, t]);

  const handleDeleteAttachment = useCallback(async () => {
    if (!confirmDeleteAtt) return;
    const aid = confirmDeleteAtt;
    setConfirmDeleteAtt(null);
    const ok = await deleteAttachment(aid);
    if (ok) {
      showToast({ type: "success", message: t("pm.task.attachmentDeleted") });
    } else {
      showToast({ type: "error", message: t("pm.task.attachmentDeleteFailed") });
    }
  }, [confirmDeleteAtt, deleteAttachment, t]);

  if (loading && !detail) {
    return (
      <div className="fixed inset-0 z-40" role="dialog" aria-modal="true">
        <div className="absolute inset-0 bg-black/30" onClick={onClose} />
        <aside className="absolute inset-y-0 right-0 w-[480px] bg-chat-area p-6 shadow-xl dark:bg-zinc-900">
          <div className="h-5 w-48 animate-pulse rounded bg-zinc-100 dark:bg-zinc-800" />
          <div className="mt-4 h-3 w-32 animate-pulse rounded bg-zinc-100 dark:bg-zinc-800" />
          <div className="mt-6 h-40 animate-pulse rounded bg-zinc-100 dark:bg-zinc-800" />
        </aside>
      </div>
    );
  }

  if (!detail) {
    return (
      <div className="fixed inset-0 z-40" role="dialog" aria-modal="true">
        <div className="absolute inset-0 bg-black/30" onClick={onClose} />
        <aside className="absolute inset-y-0 right-0 w-[480px] bg-chat-area p-6 shadow-xl dark:bg-zinc-900">
          <p className="text-xs text-zinc-500">
            {error ?? t("pm.task.taskNotFound")}
          </p>
          <button
            type="button"
            onClick={onClose}
            className="mt-3 rounded-md px-3 py-1.5 text-xs font-medium text-zinc-600 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-700"
          >
            {t("common.close") ?? "Close"}
          </button>
        </aside>
      </div>
    );
  }

  return (
    <div className="fixed inset-0 z-40" role="dialog" aria-modal="true" aria-label={detail.title}>
      <div className="absolute inset-0 bg-black/30" onClick={onClose} />
      <aside
        ref={drawerRef}
        tabIndex={-1}
        role="dialog"
        aria-modal="true"
        aria-label={detail?.title || t("pm.task.details")}
        className="absolute inset-y-0 right-0 flex w-[480px] flex-col bg-chat-area shadow-xl outline-none dark:bg-zinc-900"
      >
        {/* 头部：标题 + 徽章 + 关闭 */}
        <header className="shrink-0 border-b border-zinc-200 px-4 py-3 dark:border-zinc-700">
          <div className="flex items-start justify-between gap-2">
            <div className="min-w-0 flex-1">
              <h2 className="break-words text-sm font-semibold text-zinc-800 dark:text-zinc-100">
                {detail.title}
              </h2>
              <div className="mt-1.5 flex flex-wrap items-center gap-x-2 gap-y-1 text-[11px] text-zinc-500 dark:text-zinc-400">
                <TaskTypeIcon type={detail.type} className="text-xs" />
                <PriorityBadge priority={detail.priority} />
                <span>{statusLabel(t, detail.status)}</span>
              </div>
            </div>
            <button
              type="button"
              onClick={onClose}
              className="shrink-0 rounded-md px-1.5 py-0.5 text-sm text-zinc-400 hover:bg-zinc-200 hover:text-zinc-700 dark:hover:bg-zinc-700 dark:hover:text-zinc-200"
              aria-label={t("common.close") ?? "Close"}
            >
              ×
            </button>
          </div>

          {/* Tabs */}
          <div
            role="tablist"
            aria-label={t("pm.task.tabs")}
            className="mt-3 flex gap-1 border-b border-zinc-200 dark:border-zinc-700"
          >
            {tabs.map((tab, i) => (
              <button
                key={tab.id}
                ref={(el) => {
                  tabRefs.current[i] = el;
                }}
                type="button"
                role="tab"
                id={`pm-tab-${tab.id}`}
                aria-selected={activeTab === tab.id}
                aria-controls={`pm-panel-${tab.id}`}
                tabIndex={activeTab === tab.id ? 0 : -1}
                onClick={() => setActiveTab(tab.id)}
                onKeyDown={(e) => handleTabKeyDown(e, i)}
                className={cn(
                  "-mb-px border-b-2 px-2 py-1 text-[11px] font-medium transition-colors",
                  activeTab === tab.id
                    ? "border-[var(--color-accent)] text-zinc-800 dark:text-zinc-100"
                    : "border-transparent text-zinc-400 hover:text-zinc-600 dark:hover:text-zinc-300",
                )}
              >
                {tab.label}
              </button>
            ))}
          </div>
        </header>

        {/* 内容区 */}
        <div className="min-h-0 flex-1 overflow-y-auto px-4 py-3">
          {error && (
            <div className="mb-3 rounded-md bg-amber-50 px-3 py-2 text-xs text-amber-700 dark:bg-amber-950/40 dark:text-amber-300">
              {error}
              <button type="button" onClick={() => refresh()} className="ml-2 font-medium underline">
                {t("common.retry")}
              </button>
            </div>
          )}

          <div
            role="tabpanel"
            id={`pm-panel-${activeTab}`}
            aria-labelledby={`pm-tab-${activeTab}`}
            tabIndex={0}
          >
          {activeTab === "overview" && (
            <dl className="space-y-2 text-xs">
              <Field label={t("pm.task.assignee")} value={assigneeName ?? "—"} />
              <Field
                label={t("pm.task.dueDate")}
                value={detail.due_at ? formatDateTime(detail.due_at) : "—"}
              />
              <Field label={t("pm.task.createdBy")} value={creatorName ?? "—"} />
              <Field label={t("pm.task.created")} value={formatDateTime(detail.created_at)} />
              <Field label={t("pm.task.updated")} value={formatDateTime(detail.updated_at)} />
              {detail.parent_id && (
                <Field
                  label={t("pm.task.parentTask")}
                  value={String(detail.parent_id)}
                  className="break-all"
                />
              )}
            </dl>
          )}

          {activeTab === "description" && (
            <div className="text-xs leading-relaxed text-zinc-700 dark:text-zinc-300">
              {detail.description ? (
                <p className="whitespace-pre-wrap break-words">{detail.description}</p>
              ) : (
                <p className="italic text-zinc-400">{t("pm.noDescription")}</p>
              )}
            </div>
          )}

          {activeTab === "dependencies" && (
            <ul className="space-y-1 text-xs">
              {detail.depends_on.length === 0 ? (
                <li className="text-zinc-400">{t("pm.task.noDependencies")}</li>
              ) : (
                detail.depends_on.map((dep, i) => (
                  <li key={i} className="flex items-center gap-1.5 rounded-md bg-zinc-100 px-2 py-1.5 dark:bg-zinc-800">
                    <span aria-hidden>🔗</span>
                    <span className="break-all">{dep.task_id}</span>
                    <span className="ml-auto text-zinc-400">({dep.kind})</span>
                  </li>
                ))
              )}
            </ul>
          )}

          {activeTab === "attachments" && (
            <div className="space-y-3">
              {/* 上传按钮 */}
              <div className="flex items-center gap-2">
                <button
                  type="button"
                  disabled={uploading}
                  onClick={() => fileInputRef.current?.click()}
                  className="inline-flex items-center gap-1.5 rounded-md border border-zinc-300 bg-white px-2.5 py-1.5 text-xs font-medium text-zinc-700 transition-colors hover:bg-zinc-50 disabled:cursor-not-allowed disabled:opacity-50 dark:border-zinc-600 dark:bg-zinc-800 dark:text-zinc-200 dark:hover:bg-zinc-700"
                >
                  <span aria-hidden>⬆</span>
                  {uploading ? t("pm.task.uploading") : t("pm.task.uploadAttachment")}
                </button>
                <input
                  ref={fileInputRef}
                  type="file"
                  className="hidden"
                  aria-label={t("pm.task.uploadAttachment")}
                  onChange={async (e) => {
                    const file = e.target.files?.[0];
                    e.target.value = "";
                    if (!file) return;
                    const meta = await uploadAttachment(file);
                    if (meta) {
                      showToast({ type: "success", message: t("pm.task.attachmentUploaded") });
                    } else {
                      showToast({ type: "error", message: t("pm.task.attachmentUploadFailed") });
                    }
                  }}
                />
              </div>

              {attachments.length === 0 ? (
                <p className="text-xs text-zinc-400">{t("pm.task.noAttachments")}</p>
              ) : (
                <ul className="grid grid-cols-3 gap-2">
                  {attachments.map((att) => (
                    <li
                      key={att.id}
                      className="group relative overflow-hidden rounded-md border border-zinc-200 bg-card-surface dark:border-zinc-700"
                    >
                      {att.kind === "image" ? (
                        <a href={attachmentUrl(att.id)} target="_blank" rel="noreferrer">
                          <img
                            src={attachmentUrl(att.id, { thumb: true })}
                            alt={att.filename}
                            className="h-20 w-full object-cover"
                            loading="lazy"
                          />
                        </a>
                      ) : (
                        <a
                          href={attachmentUrl(att.id, { download: true })}
                          className="flex h-20 flex-col items-center justify-center gap-0.5 text-zinc-500 hover:bg-zinc-50 dark:text-zinc-400 dark:hover:bg-zinc-700"
                        >
                          <span className="text-lg" aria-hidden>📄</span>
                          <span className="max-w-full truncate px-1 text-[10px]">{att.filename}</span>
                        </a>
                      )}
                      {/* 删除按钮（opacity 模式：hover/focus 可见，DOM 常驻可聚焦） */}
                      <button
                        type="button"
                        aria-label={t("pm.task.deleteAttachment")}
                        title={t("pm.task.deleteAttachment")}
                        onClick={() => setConfirmDeleteAtt(att.id)}
                        className="absolute right-1 top-1 flex h-5 w-5 items-center justify-center rounded-full bg-black/50 text-[10px] text-white opacity-0 transition-opacity hover:bg-red-600 focus-visible:opacity-100 group-hover:opacity-100"
                      >
                        ✕
                      </button>
                      <div className="px-1.5 py-1 text-[9px] text-zinc-400">
                        <span className="block truncate">{att.filename}</span>
                        <span>{formatBytes(att.size)}</span>
                      </div>
                    </li>
                  ))}
                </ul>
              )}
            </div>
          )}

          {activeTab === "subtasks" && (
            <div>
              <SubtaskTree rootId={detail.id} onSelect={(id) => openTask(id)} />
              <button
                type="button"
                onClick={() => onAddSubtask?.(detail.id)}
                className="mt-2 w-full rounded-md border border-dashed border-zinc-300 px-3 py-1.5 text-xs text-zinc-500 hover:border-[var(--color-accent)] hover:text-[var(--color-accent)] dark:border-zinc-600"
              >
                + {t("pm.task.addSubtask")}
              </button>
            </div>
          )}
          </div>
        </div>

        {/* 底部操作 */}
        <footer className="flex shrink-0 items-center gap-2 border-t border-zinc-200 px-4 py-2.5 dark:border-zinc-700">
          {onEdit && (
            <button
              type="button"
              onClick={() => onEdit(detail)}
              className="rounded-md px-3 py-1.5 text-xs font-medium text-zinc-700 hover:bg-zinc-100 dark:text-zinc-300 dark:hover:bg-zinc-700"
            >
              {t("pm.task.edit")}
            </button>
          )}
          <button
            type="button"
            onClick={() => setConfirmDelete(true)}
            className="rounded-md px-3 py-1.5 text-xs font-medium text-red-600 hover:bg-red-50 dark:text-red-400 dark:hover:bg-red-950/40"
          >
            {t("pm.task.delete")}
          </button>

          <div className="ml-auto flex items-center gap-1.5">
            <StatusMoveButton
              task={detail}
              onMoved={refresh}
            />
          </div>
        </footer>
      </aside>

      <ConfirmDialog
        open={confirmDelete}
        title={t("pm.task.deleteTaskTitle")}
        message={`${t("pm.task.deleteTaskCascadeDesc")} "${detail.title}"?`}
        confirmLabel={t("common.delete")}
        destructive
        onConfirm={handleDelete}
        onCancel={() => setConfirmDelete(false)}
      />

      <ConfirmDialog
        open={confirmDeleteAtt !== null}
        title={t("pm.task.deleteAttachmentTitle")}
        message={t("pm.task.deleteAttachmentDesc")}
        confirmLabel={t("common.delete")}
        destructive
        onConfirm={handleDeleteAttachment}
        onCancel={() => setConfirmDeleteAtt(null)}
      />
    </div>
  );
}

function Field({ label, value, className }: { label: string; value: string; className?: string }) {
  return (
    <div className="flex gap-2">
      <dt className="w-20 shrink-0 text-zinc-400 dark:text-zinc-500">{label}</dt>
      <dd className={cn("min-w-0 flex-1 text-zinc-700 dark:text-zinc-300", className)}>{value}</dd>
    </div>
  );
}

/** 状态快速流转按钮组：按当前状态显示下一步动作 */
function StatusMoveButton({ task, onMoved }: { task: PmTaskResponse; onMoved: () => void }) {
  const { t } = useTranslation();
  const moveTask = usePmBoardStore((s) => s.moveTask);

  const next: Partial<Record<TaskStatus, TaskStatus>> = {
    pending: "in_progress",
    in_progress: "submitted",
    submitted: "done",
    rejected: "in_progress",
  };
  const target = next[task.status];
  if (!target) return null;

  const label =
    task.status === "submitted"
      ? t("pm.task.approve")
      : task.status === "rejected"
        ? t("pm.task.restart")
        : `${t("pm.task.moveTo")} ${t(`pm.board.${target === "in_progress" ? "inProgress" : target}`)}`;

  const handle = async () => {
    await moveTask(task.id, target);
    onMoved();
  };

  return (
    <button
      type="button"
      onClick={handle}
      className="rounded-md bg-zinc-800 px-3 py-1.5 text-xs font-medium text-white hover:bg-zinc-700 dark:bg-zinc-700 dark:hover:bg-zinc-600"
    >
      {label}
    </button>
  );
}
