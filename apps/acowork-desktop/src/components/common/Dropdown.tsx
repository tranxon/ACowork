/**
 * Dropdown — standard <select> wrapper.
 *
 * Unifies the ~13 hand-written <select>s across the app that previously
 * each inlined the same `appearance-none` + SVG-arrow `style` block. Two
 * visual variants (`standard` / `small`) match the two historical styles;
 * callers should NOT need to override the visual variant prop in normal
 * use.
 *
 * Design notes:
 * - Accepts a plain `options: { value, label }[]` instead of children
 *   `<option>` nodes — keeps call sites compact and lets the component
 *   own the <option> rendering.
 * - `placeholder` is rendered as an optional first <option> (disabled +
 *   hidden by default) used by ProfileTab/AgentSetupTab for the
 *   "no preset selected" state.
 * - All `<select>` HTML props pass through via `...rest`.
 * - `className` extends (not replaces) the variant's base class.
 * - SVG arrow is identical to the prior hand-rolled sites:
 *   stroke=#6b7280 (zinc-500), 1.5 / 1.2 em size, position right 0.5 / 0.25 rem.
 */
import * as React from "react";
import { cn } from "../../lib/utils";

export interface DropdownOption {
  value: string;
  label: React.ReactNode;
}

export interface DropdownPlaceholder {
  value: string;
  label: React.ReactNode;
  /** Whether the placeholder option can be re-picked by the user. */
  selectable?: boolean;
}

export type DropdownSize = "standard" | "small";

export interface DropdownProps
  extends Omit<React.SelectHTMLAttributes<HTMLSelectElement>, "size" | "onChange"> {
  value: string;
  onChange: (value: string) => void;
  options: DropdownOption[];
  placeholder?: DropdownPlaceholder;
  size?: DropdownSize;
  /** Extra className layered on top of the variant base. */
  className?: string;
}

const ARROW_SVG =
  "url(\"data:image/svg+xml,%3csvg xmlns='http://www.w3.org/2000/svg' fill='none' viewBox='0 0 20 20'%3e%3cpath stroke='%236b7280' stroke-linecap='round' stroke-linejoin='round' stroke-width='1.5' d='M6 8l4 4 4-4'/%3e%3c/svg%3e\")";

const baseBySize: Record<DropdownSize, { className: string; style: React.CSSProperties }> = {
  standard: {
    className:
      "w-full appearance-none rounded border border-zinc-200 bg-modal-surface px-2.5 py-1.5 text-xs text-zinc-800 focus:border-zinc-400 focus:outline-none focus:ring-1 focus:ring-zinc-400 dark:border-zinc-700 dark:text-zinc-200",
    style: {
      backgroundImage: ARROW_SVG,
      backgroundPosition: "right 0.5rem center",
      backgroundRepeat: "no-repeat",
      backgroundSize: "1.5em 1.5em",
      paddingRight: "2rem",
      appearance: "none",
      WebkitAppearance: "none",
      MozAppearance: "none",
    },
  },
  small: {
    className:
      "h-7 appearance-none rounded-md border border-zinc-200 bg-modal-surface px-1.5 text-[11px] text-zinc-700 outline-none transition-colors focus:border-[var(--color-accent)] dark:border-zinc-600 dark:text-zinc-300",
    style: {
      backgroundImage: ARROW_SVG,
      backgroundPosition: "right 0.25rem center",
      backgroundRepeat: "no-repeat",
      backgroundSize: "1.2em 1.2em",
      paddingRight: "1.25rem",
      appearance: "none",
      WebkitAppearance: "none",
      MozAppearance: "none",
    },
  },
};

export function Dropdown({
  value,
  onChange,
  options,
  placeholder,
  size = "standard",
  className,
  disabled,
  ...rest
}: DropdownProps) {
  const base = baseBySize[size];

  return (
    <select
      value={value}
      onChange={(e) => onChange(e.target.value)}
      disabled={disabled}
      className={cn(base.className, className)}
      style={base.style}
      {...rest}
    >
      {placeholder && (
        <option value={placeholder.value} disabled={!placeholder.selectable} hidden={!placeholder.selectable}>
          {placeholder.label}
        </option>
      )}
      {options.map((o) => (
        <option key={o.value} value={o.value}>
          {o.label}
        </option>
      ))}
    </select>
  );
}