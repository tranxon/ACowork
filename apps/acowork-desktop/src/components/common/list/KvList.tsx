//! KvList — the unified monospace key/value detail block.
//!
//! Used by expanded rows to show read-only metadata without the cramped
//! wrapping of a fixed multi-column table in a narrow panel (compression
//! history). One `label: value` pair per line, horizontally scrollable.
//! Matches the previous CompressionHistoryCard detail pill exactly.
import type { ReactNode } from "react";
import { cn } from "../../../lib/utils";

export interface KvListProps {
  rows: { k: ReactNode; v: ReactNode }[];
  className?: string;
}

export function KvList({ rows, className }: KvListProps) {
  return (
    <div
      className={cn(
        "overflow-x-auto rounded-md border border-zinc-200 bg-zinc-100/60 px-2 py-1 font-mono text-[10px] leading-4 text-zinc-500 dark:border-zinc-700 dark:bg-zinc-800/40 dark:text-zinc-400",
        className,
      )}
    >
      {rows.map((row, i) => (
        <div key={i} className="whitespace-nowrap">
          <span className="text-zinc-400 dark:text-zinc-500">{row.k}: </span>
          {row.v}
        </div>
      ))}
    </div>
  );
}
