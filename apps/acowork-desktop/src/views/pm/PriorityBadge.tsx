/**
 * PriorityBadge — 优先级徽章（颜色 + 文字双编码，WCAG AA）。
 * 对齐 UX 设计 §5.2 / §8.4：不依赖纯颜色传达信息。
 */

import { cn } from "../../lib/utils";
import type { Priority } from "../../lib/pm-types";

const STYLES: Record<Priority, string> = {
  urgent: "bg-red-100 text-red-700 dark:bg-red-950/60 dark:text-red-400",
  high: "bg-orange-100 text-orange-700 dark:bg-orange-950/60 dark:text-orange-400",
  normal: "bg-blue-100 text-blue-700 dark:bg-blue-950/60 dark:text-blue-400",
  low: "bg-zinc-100 text-zinc-600 dark:bg-zinc-800 dark:text-zinc-400",
};

const LABELS: Record<Priority, string> = {
  urgent: "URGENT",
  high: "HIGH",
  normal: "MEDIUM",
  low: "LOW",
};

export function PriorityBadge({
  priority,
  className,
}: {
  priority: Priority;
  className?: string;
}) {
  return (
    <span
      className={cn(
        "inline-flex items-center rounded px-1 py-px text-[9px] font-semibold uppercase tracking-wide",
        STYLES[priority],
        className,
      )}
      aria-label={`${LABELS[priority]} priority`}
    >
      {LABELS[priority]}
    </span>
  );
}
