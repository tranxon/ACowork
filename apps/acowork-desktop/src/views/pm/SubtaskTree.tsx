/**
 * SubtaskTree — 子任务树（T2-6）。
 *
 * 对齐 UX 设计 §3.4：递归展示子任务（最多 5 级），支持折叠。
 * - 每行：折叠箭头 + 完成状态 + 标题 + 状态
 * - 子任务从 boardStore.childrenOf 递归取（同一看板数据源，无需额外请求）
 * - 点击行 → 切换到该子任务详情（同 Drawer 内导航）
 */

import { useState } from "react";
import { usePmBoardStore } from "../../stores/pm/boardStore";
import { useTranslation } from "../../i18n/useTranslation";
import { TaskTypeIcon } from "./TaskTypeIcon";
import { cn } from "../../lib/utils";
import type { PmTaskResponse, TaskStatus } from "../../lib/pm-types";

const STATUS_STYLE: Partial<Record<TaskStatus, string>> = {
  done: "text-green-600 dark:text-green-400",
  in_progress: "text-blue-600 dark:text-blue-400",
  submitted: "text-amber-600 dark:text-amber-400",
  rejected: "text-red-600 dark:text-red-400",
  cancelled: "text-zinc-400 dark:text-zinc-500",
  pending: "text-zinc-500 dark:text-zinc-400",
};

function StatusDot({ status }: { status: TaskStatus }) {
  return (
    <span
      className={cn("h-1.5 w-1.5 shrink-0 rounded-full bg-current", STATUS_STYLE[status])}
      aria-hidden
    />
  );
}

function SubtaskRow({
  task,
  depth,
  onSelect,
}: {
  task: PmTaskResponse;
  depth: number;
  onSelect: (taskId: string) => void;
}) {
  const { t } = useTranslation();
  const children = usePmBoardStore((s) => s.childrenOf(task.id));
  const [collapsed, setCollapsed] = useState(false);
  const hasChildren = children.length > 0;

  return (
    <li className="space-y-0.5">
      <div
        className={cn(
          "flex w-full cursor-pointer items-center gap-1.5 rounded px-1.5 py-1 text-left text-xs hover:bg-zinc-100 dark:hover:bg-zinc-800",
          depth > 0 && "ml-3",
        )}
        style={{ paddingLeft: 6 + depth * 14 }}
        onClick={() => onSelect(task.id)}
        role="button"
        tabIndex={0}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            onSelect(task.id);
          }
        }}
        aria-label={task.title}
      >
        {hasChildren ? (
          <button
            type="button"
            className="w-4 shrink-0 text-center text-[10px] text-zinc-400 hover:text-zinc-600 dark:hover:text-zinc-300"
            onClick={(e) => {
              e.stopPropagation();
              setCollapsed((v) => !v);
            }}
            aria-label={collapsed ? t("pm.task.expand") : t("pm.task.collapse")}
            aria-expanded={!collapsed}
          >
            {collapsed ? "▸" : "▾"}
          </button>
        ) : (
          <span className="w-4 shrink-0" aria-hidden />
        )}
        <StatusDot status={task.status} />
        <TaskTypeIcon type={task.type} className="shrink-0 text-[10px]" />
        <span
          className={cn(
            "min-w-0 flex-1 truncate",
            task.status === "done" && "text-zinc-400 line-through dark:text-zinc-500",
          )}
        >
          {task.title}
        </span>
      </div>

      {hasChildren && !collapsed && (
        <ul className="space-y-0.5">
          {children.map((child) => (
            <SubtaskRow key={child.id} task={child} depth={depth + 1} onSelect={onSelect} />
          ))}
        </ul>
      )}
    </li>
  );
}

export function SubtaskTree({
  rootId,
  onSelect,
}: {
  rootId: string;
  onSelect: (taskId: string) => void;
}) {
  const root = usePmBoardStore((s) => s.tasks.find((t) => t.id === rootId));
  const children = usePmBoardStore((s) => s.childrenOf(rootId));

  if (!root || children.length === 0) {
    return (
      <p className="px-1 py-2 text-xs text-zinc-400 dark:text-zinc-500">
        {root ? "暂无子任务" : ""}
      </p>
    );
  }

  return (
    <ul className="space-y-0.5">
      {children.map((child) => (
        <SubtaskRow key={child.id} task={child} depth={0} onSelect={onSelect} />
      ))}
    </ul>
  );
}
