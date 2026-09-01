/**
 * TaskCard — 看板任务卡片（T2-5）。
 *
 * 对齐 UX 设计 §3.4：
 * - 类型图标 + 标题
 * - 优先级徽章（PriorityBadge）
 * - 负责人（assignee 显示名，从 agentStore 解析）
 * - 截止日期（逾期红色高亮）
 * - 子任务计数（childrenOf）
 * - 阻塞标记（is_blocked → 🔒）
 * - dnd-kit useDraggable 作为拖拽源（T2-8 接线拖动流转）
 */

import { useState } from "react";
import { useDraggable, useDroppable } from "@dnd-kit/core";
import { CSS } from "@dnd-kit/utilities";
import { useAgentStore } from "../../stores/agentStore";
import { usePmBoardStore } from "../../stores/pm/boardStore";
import { usePmHealthStore } from "../../stores/pm/healthStore";
import { useTranslation } from "../../i18n/useTranslation";
import { PriorityBadge } from "./PriorityBadge";
import { TaskTypeIcon } from "./TaskTypeIcon";
import { RejectDialog } from "./RejectDialog";
import { cn } from "../../lib/utils";
import type { PmTaskResponse } from "../../lib/pm-types";

/** 解析 agent id → 显示名（meta.display_name ?? meta.name ?? id） */
function resolveAgentName(agents: Record<string, { meta?: { display_name?: string; name?: string } }>, id: string | null): string | null {
  if (!id) return null;
  const a = agents[id];
  if (!a?.meta) return id;
  return a.meta.display_name || a.meta.name || id;
}

interface TaskCardProps {
  task: PmTaskResponse;
  onOpenTask: (taskId: string) => void;
  className?: string;
  /** 嵌套层级（用于缩进，0 = 列内根任务） */
  depth?: number;
}

export function TaskCard({ task, onOpenTask, className, depth = 0 }: TaskCardProps) {
  const { t } = useTranslation();
  const agents = useAgentStore((s) => s.agents);
  const healthy = usePmHealthStore((s) => s.healthy);
  const childCount = usePmBoardStore((s) => s.childrenOf(task.id).length);
  const [rejecting, setRejecting] = useState(false);

  // 离线（healthy === false）时禁用拖拽源 —— 看板只读
  const draggableDisabled = healthy === false;
  const { attributes, listeners, setNodeRef: setDragRef, transform, isDragging } = useDraggable({
    id: task.id,
    data: { type: "task", taskId: task.id, status: task.status, projectId: task.project_id },
    disabled: draggableDisabled,
  });

  // T2-9：卡片同时作为 droppable —— 拖到卡片上 → reparent 为子任务
  const { setNodeRef: setDropRef, isOver: dropOver } = useDroppable({
    id: `drop-${task.id}`,
    data: { type: "task", taskId: task.id, status: task.status },
    disabled: draggableDisabled,
  });

  const setRefs = (el: HTMLElement | null) => {
    setDragRef(el);
    setDropRef(el);
  };

  const style = transform
    ? { transform: CSS.Transform.toString(transform) }
    : undefined;

  const assigneeName = resolveAgentName(agents, task.assignee);
  const creatorName = resolveAgentName(agents, task.created_by);

  // 截止日期格式化：MM-DD（跨年带年份），逾期红色
  const dueLabel = task.due_at ? formatDue(task.due_at) : null;
  const overdue = task.due_at ? isOverdue(task.due_at, task.status) : false;

  const handleApprove = async () => {
    await usePmBoardStore.getState().reviewTask(task.id, true);
  };

  const handleReject = async (reason?: string) => {
    setRejecting(false);
    await usePmBoardStore.getState().reviewTask(task.id, false, reason);
  };

  return (
    <>
      <div
        ref={setRefs}
        style={style}
        {...attributes}
        {...listeners}
        role="button"
        tabIndex={0}
        onClick={() => onOpenTask(task.id)}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            onOpenTask(task.id);
          }
        }}
        className={cn(
          "group w-full rounded-md border border-zinc-200 bg-card-surface px-2.5 py-2 text-left shadow-sm transition-colors",
          "hover:border-zinc-300 hover:shadow dark:border-zinc-700 dark:bg-zinc-800 dark:hover:border-zinc-600",
          "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-accent)]",
          task.status === "submitted" && "border-amber-300 bg-amber-50/60 dark:border-amber-700/60 dark:bg-amber-950/30",
          isDragging && "opacity-40",
          dropOver && "ring-2 ring-[var(--color-accent)]",
          depth > 0 && "ml-4",
          className,
        )}
        aria-label={task.title}
      >
        <div className="flex items-start gap-1.5">
          <TaskTypeIcon type={task.type} className="mt-0.5 text-xs leading-none" />
          <span className="min-w-0 flex-1 text-xs font-medium leading-snug text-zinc-800 dark:text-zinc-100">
            {task.title}
          </span>
        </div>

        <div className="mt-1.5 flex flex-wrap items-center gap-x-2 gap-y-1 text-[10px] text-zinc-500 dark:text-zinc-400">
          <PriorityBadge priority={task.priority} />

          {task.is_blocked && (
            <span className="inline-flex items-center gap-0.5 text-red-600 dark:text-red-400" title="blocked">
              🔒
            </span>
          )}

          {assigneeName && (
            <span className="inline-flex min-w-0 items-center gap-0.5">
              <span aria-hidden>👤</span>
              <span className="max-w-24 truncate">{assigneeName}</span>
            </span>
          )}

          {dueLabel && (
            <span
              className={cn(
                "inline-flex items-center gap-0.5",
                overdue ? "font-medium text-red-600 dark:text-red-400" : "",
              )}
            >
              <span aria-hidden>📅</span>
              {dueLabel}
            </span>
          )}

          {childCount > 0 && (
            <span className="inline-flex items-center gap-0.5" title="subtasks">
              <span aria-hidden>⊞</span>
              {childCount}
            </span>
          )}
        </div>

        {/* T2-11：待审核卡片 —— 创建者 + 批准/拒绝 inline 操作 */}
        {task.status === "submitted" && (
          <div className="mt-2 border-t border-amber-200/70 pt-1.5 dark:border-amber-800/40">
            {creatorName && (
              <p className="mb-1.5 text-[10px] text-zinc-400 dark:text-zinc-500">
                {t("pm.review.createdByLabel")}{" "}
                <span className="font-medium text-amber-700 dark:text-amber-300">{creatorName}</span>
              </p>
            )}
            <div className="flex items-center gap-1.5">
              <button
                type="button"
                disabled={draggableDisabled}
                onClick={(e) => {
                  e.stopPropagation();
                  void handleApprove();
                }}
                onKeyDown={(e) => e.stopPropagation()}
                className="rounded bg-green-600 px-2 py-0.5 text-[10px] font-medium text-white hover:bg-green-500 disabled:cursor-not-allowed disabled:opacity-40 dark:bg-green-700 dark:hover:bg-green-600"
              >
                ✓ {t("pm.review.approve")}
              </button>
              <button
                type="button"
                disabled={draggableDisabled}
                onClick={(e) => {
                  e.stopPropagation();
                  setRejecting(true);
                }}
                onKeyDown={(e) => e.stopPropagation()}
                className="rounded bg-red-600 px-2 py-0.5 text-[10px] font-medium text-white hover:bg-red-500 dark:bg-red-700 dark:hover:bg-red-600"
              >
                ✗ {t("pm.review.reject")}
              </button>
            </div>
          </div>
        )}
      </div>

      {/* 拒绝确认 + 可选理由 */}
      <RejectDialog
        open={rejecting}
        taskTitle={task.title}
        onConfirm={handleReject}
        onCancel={() => setRejecting(false)}
      />
    </>
  );
}

/** 截止日期短格式：本年内 MM-DD，跨年 YYYY-MM-DD */
function formatDue(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso.slice(0, 10);
  const now = new Date();
  const sameYear = d.getFullYear() === now.getFullYear();
  const mm = String(d.getMonth() + 1).padStart(2, "0");
  const dd = String(d.getDate()).padStart(2, "0");
  return sameYear ? `${mm}-${dd}` : `${d.getFullYear()}-${mm}-${dd}`;
}

/** 逾期判断：有截止且未完成/未提交的已过截止时间 */
function isOverdue(iso: string, status: PmTaskResponse["status"]): boolean {
  if (status === "done" || status === "cancelled" || status === "rejected") return false;
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return false;
  return d.getTime() < Date.now();
}
