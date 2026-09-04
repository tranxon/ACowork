/**
 * TaskTypeIcon — 任务类型图标（emoji，带 aria-label）。
 * 对齐 UX 设计 §3.3：🐛 bug / ✨ feature / 🧹 chore / 📋 task / 🚩 checkpoint / 🏁 milestone。
 */

import type { TaskType } from "../../lib/pm-types";

const ICONS: Record<TaskType, { icon: string; label: string }> = {
  bug: { icon: "🐛", label: "Bug" },
  feature: { icon: "✨", label: "Feature" },
  chore: { icon: "🧹", label: "Chore" },
  task: { icon: "📋", label: "Task" },
  checkpoint: { icon: "🚩", label: "Checkpoint" },
  milestone: { icon: "🏁", label: "Milestone" },
};

export function TaskTypeIcon({
  type,
  className,
}: {
  type: TaskType;
  className?: string;
}) {
  const meta = ICONS[type];
  return (
    <span
      className={className}
      role="img"
      aria-label={meta.label}
      title={meta.label}
    >
      {meta.icon}
    </span>
  );
}
