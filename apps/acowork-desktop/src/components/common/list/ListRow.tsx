//! ListRow — the unified single list row.
//!
//! Standardizes the row chrome that previously drifted between panels:
//! full-width flex layout, uniform padding, the hover / selected fills,
//! keyboard semantics when clickable, and the disabled (50% opacity) state.
//!
//! Fill tones step one level deeper per surface (light mode shown; the
//! dark scale is the zinc-700/800 translucent fills further below):
//!   card  — hover zinc-50  / selected zinc-100
//!   inset — hover zinc-100 / selected zinc-200
//!
//! The caller owns the middle column (`children`) — single-line label
//! rows, two-line title+description rows, and rich master-detail rows
//! (memory) all compose on top of the same base. `leading` and
//! `trailing` slots keep icons and switches aligned across every list.
//!
//! `surface` keeps hover/selected fills legible on the surface the row
//! sits on: "card" (default) is the raised list surface; "inset" is the
//! expanded sub-surface revealed by a collapsed card, where the card
//! fills would not read on the darker inset body — so the same zinc
//! scale steps one level deeper (see the tone table above).
import type { ReactNode } from "react";
import { cn } from "../../../lib/utils";

export interface ListRowProps {
  /** Leading icon / dot / avatar. Auto-shrinks, left of the content. */
  leading?: ReactNode;
  /** Middle content column (min-w-0 flex-1). Free-form by design. */
  children: ReactNode;
  /** Right-side action slot (Switch, icon buttons…). */
  trailing?: ReactNode;
  /** Selection highlight (e.g. memory master-detail rows). */
  selected?: boolean;
  /** Renders a disabled row at 50% opacity (e.g. tool without API key). */
  disabled?: boolean;
  /** Horizontal padding: "default" = px-3 flush row; "nested" = deep
   *  left indent (pl-10) for rows that are children of an expanded
   *  parent (MCP server → tool rows). */
  padding?: "default" | "nested";
  /** Which surface the row sits on — governs hover/selected fills:
   *  "card" (raised list surface) or "inset" (expanded sub-surface). */
  surface?: "card" | "inset";
  /** When provided the row renders as a <button> with click semantics. */
  onClick?: () => void;
  /** Accessible label for clickable rows (aria-label). */
  ariaLabel?: string;
  /** Extra classes appended to the row. */
  className?: string;
}

export function ListRow({
  leading,
  children,
  trailing,
  selected,
  disabled,
  padding = "default",
  surface = "card",
  onClick,
  ariaLabel,
  className,
}: ListRowProps) {
  const base = cn(
    "flex w-full items-center gap-2 py-1.5 text-left transition-colors",
    padding === "nested" ? "pl-10 pr-3" : "px-3",
    onClick && !disabled && "cursor-pointer",
    disabled && "opacity-50",
    !disabled && surface === "inset"
      ? selected
        ? "bg-zinc-200 hover:bg-zinc-200 dark:bg-zinc-700/60 dark:hover:bg-zinc-700/60"
        : "hover:bg-zinc-100 dark:hover:bg-zinc-700/40"
      : !disabled && (selected
        ? "bg-zinc-100 hover:bg-zinc-100 dark:bg-zinc-800 dark:hover:bg-zinc-800"
        : "hover:bg-zinc-50 dark:hover:bg-zinc-800/50"),
    className,
  );

  const content = (
    <>
      {leading != null && <span className="shrink-0">{leading}</span>}
      <div className="min-w-0 flex-1">{children}</div>
      {trailing != null && <span className="shrink-0">{trailing}</span>}
    </>
  );

  if (onClick) {
    return (
      <button type="button" onClick={onClick} disabled={disabled} aria-label={ariaLabel} className={base}>
        {content}
      </button>
    );
  }
  return <div className={base}>{content}</div>;
}
