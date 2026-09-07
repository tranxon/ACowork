//! EmptyState — the unified empty-list placeholder.
//!
//! Renders inside a ListBox; the caller supplies the container so the
//! box chrome (border/bg or not) stays consistent with nearby states.
import type { ReactNode } from "react";
import { cn } from "../../../lib/utils";

export interface EmptyStateProps {
  message: ReactNode;
  icon?: ReactNode;
  className?: string;
}

export function EmptyState({ message, icon, className }: EmptyStateProps) {
  return (
    <div
      className={cn(
        "flex items-center gap-1.5 px-3 py-2 text-[10px] text-zinc-400 dark:text-zinc-500",
        className,
      )}
    >
      {icon != null && <span className="shrink-0">{icon}</span>}
      <span className="min-w-0 flex-1">{message}</span>
    </div>
  );
}
