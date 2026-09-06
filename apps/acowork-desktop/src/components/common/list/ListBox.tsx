//! ListBox — the unified list container for the ACowork desktop app.
//!
//! One grammar for every list in the app (right-panel tabs, settings,
//! harness, agents, skills): either a raised "box" card (`bg-panel-block`
//! + border, used on top of the right-panel / page surfaces) or a
//! borderless "plain" stack that sits directly on the parent surface.
//!
//! Hairline dividers between direct row children are part of the container
//! contract. The tone is uniform across surfaces (zinc-200 / dark
//! zinc-700): on `panel-block` (zinc-800 in dark) a 700 hairline stays
//! visible, and on the darker right-panel surface it reads as a clear
//! separator too — an 800 hairline would all but disappear there. This is
//! the fix for the previously near-invisible snapshot / compression rows.
import type { HTMLAttributes } from "react";
import { cn } from "../../../lib/utils";

export interface ListBoxProps extends HTMLAttributes<HTMLDivElement> {
  /** box = bordered card on bg-panel-block; plain = bare stack. Default: "box". */
  variant?: "box" | "plain";
  /** Render hairline dividers between row children. Default: true. */
  dividers?: boolean;
  /** Cap the list height (px) and make it scroll internally. */
  maxHeight?: number;
}

export function ListBox({
  variant = "box",
  dividers = true,
  maxHeight,
  className,
  children,
  ...rest
}: ListBoxProps) {
  return (
    <div
      className={cn(
        "min-w-0",
        variant === "box" &&
          "rounded-md border border-zinc-200 bg-panel-block dark:border-zinc-700",
        dividers && "divide-y divide-zinc-200 dark:divide-zinc-700",
        className,
      )}
      style={maxHeight !== undefined ? { maxHeight, overflowY: "auto" } : undefined}
      {...rest}
    >
      {children}
    </div>
  );
}
