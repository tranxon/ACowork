//! ExpandableRow — the unified collapsible list item (2-level lists).
//!
//! Parent rows that open a nested body: MCP server -> tools, the PROMPT
//! section, debug snapshot iterations, compression-history rows. The
//! chevron is a single ChevronRight icon that rotates 90° on open (one
//! affordance instead of the previous Right/Down icon swaps). When the
//! row has no expandable body (`disabled`), the chevron slot is reserved
//! so titles stay column-aligned but nothing is rendered as clickable.
//!
//! Expand/collapse state stays with the caller (open + onToggle).
//!
//! `variant`:
//!   - "row"     (default) — transparent, hover-highlight on the header.
//!   - "section"           — persistent grouping strip for rows that sit
//!                           directly on the CARD surface (not inside an
//!                           expanded body): bg zinc-100 / dark
//!                           zinc-700/60. Rows inside an expanded
//!                           sub-surface must use "row" + surface="inset"
//!                           instead — a fixed strip there would blend
//!                           with the card/header tone.
//!
//! The body renders with no implicit chrome; add the hairline + padding
//! via `bodyClassName` (see the surface rule in ListBox). When the body
//! is an expanded sub-surface, pass a deeper tone via `bodyClassName`
//! (e.g. `rounded-b-md border-t … bg-zinc-50 dark:bg-zinc-900/60`) so
//! level-1 header vs level-2 rows stay visually separated, and set the
//! header `surface="inset"` so its hover stays legible above that body.
import type { KeyboardEvent, ReactNode } from "react";
import { ChevronRight } from "lucide-react";
import { cn } from "../../../lib/utils";

export interface ExpandableRowProps {
  open: boolean;
  onToggle: () => void;
  /** Primary header line (11px medium). */
  title: ReactNode;
  /** Secondary inline info right after the title (badges / counts). */
  meta?: ReactNode;
  /** Secondary line under the title (9px small). */
  description?: ReactNode;
  /** Right-side slot. Interactive elements inside MUST stopPropagation. */
  trailing?: ReactNode;
  /** "row" (default) or "section" (persistent strip, Snapshot-style). */
  variant?: "row" | "section";
  /** Which surface the row sits on — hover fill tone ("card" default,
   *  "inset" for rows on an expanded sub-surface). */
  surface?: "card" | "inset";
  /** No expandable body: hide the chevron, header becomes inert. */
  disabled?: boolean;
  /** Extra classes on the header. */
  className?: string;
  /** Extra classes on the expansion body container. */
  bodyClassName?: string;
  /** aria-label for the header toggle. */
  ariaLabel?: string;
  /** Expansion body (rendered only when open). */
  children?: ReactNode;
}

export function ExpandableRow({
  open,
  onToggle,
  title,
  meta,
  description,
  trailing,
  variant = "row",
  surface = "card",
  disabled,
  className,
  bodyClassName,
  ariaLabel,
  children,
}: ExpandableRowProps) {
  const interactive = !disabled;

  const handleKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (interactive && (e.key === "Enter" || e.key === " ")) {
      e.preventDefault();
      onToggle();
    }
  };

  return (
    <div className="flex min-h-0 min-w-0 flex-1 flex-col">
      <div
        role="button"
        tabIndex={interactive ? 0 : -1}
        aria-expanded={interactive ? open : undefined}
        aria-label={ariaLabel}
        onClick={() => interactive && onToggle()}
        onKeyDown={interactive ? handleKeyDown : undefined}
        className={cn(
          "flex w-full items-center gap-2 text-left transition-colors",
          variant === "section"
            ? "min-h-[30px] rounded-md bg-zinc-100 px-2.5 hover:bg-zinc-200/80 dark:bg-zinc-700/60 dark:hover:bg-zinc-600/50"
            : surface === "inset"
              ? "min-h-[36px] px-3 hover:bg-zinc-400 dark:hover:bg-zinc-700/40"
              : "min-h-[36px] px-3 hover:bg-zinc-50 dark:hover:bg-zinc-800/50",
          interactive && "cursor-pointer",
          className,
        )}
      >
        {interactive ? (
          <ChevronRight
            aria-hidden
            className={cn(
              "h-3.5 w-3.5 shrink-0 text-zinc-400 transition-transform duration-150 dark:text-zinc-500",
              open && "rotate-90",
            )}
          />
        ) : (
          <span aria-hidden className="h-3.5 w-3.5 shrink-0" />
        )}
        <div className="min-w-0 flex-1">
          <div className="flex min-w-0 items-center gap-1.5">
            <span className="truncate text-[11px] font-medium text-zinc-700 dark:text-zinc-300">
              {title}
            </span>
            {meta != null && <span className="flex shrink-0 items-center gap-1">{meta}</span>}
          </div>
          {description != null && (
            <div className="truncate text-[9px] leading-tight text-zinc-400 dark:text-zinc-500">
              {description}
            </div>
          )}
        </div>
        {trailing != null && <span className="shrink-0">{trailing}</span>}
      </div>
      {open && (
        <div className={cn("min-w-0", bodyClassName)}>{children}</div>
      )}
    </div>
  );
}
