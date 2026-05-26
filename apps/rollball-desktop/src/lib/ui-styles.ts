/**
 * Global UI style constants for consistent component appearance.
 * All input fields, buttons, and interactive elements should use these tokens.
 */

// ── Input field styles ──────────────────────────────────────────────

/** Standard input field (text, number, etc.) */
export const inputBase =
  "w-full rounded-md border border-zinc-200 px-3 py-[var(--ui-input-py)] text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200";

/** Read-only input field */
export const inputReadonly =
  "rounded-md border border-zinc-200 bg-zinc-50 px-3 py-[var(--ui-input-py)] text-xs dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-400";

/** Select/dropdown field */
export const selectBase =
  "appearance-none rounded-md border border-zinc-200 bg-white px-3 py-[var(--ui-input-py)] text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200 bg-[url('data:image/svg+xml,%3csvg xmlns=%27http://www.w3.org/2000/svg%27 fill=%27none%27 viewBox=%270 0 20 20%27%3e%3cpath stroke=%27%236b7280%27 stroke-linecap=%27round%27 stroke-linejoin=%27round%27 stroke-width=%271.5%27 d=%27M6 8l4 4 4-4%27/%3e%3c/svg%3e')] bg-[position:right_0.5rem_center] bg-no-repeat bg-[length:1.5em_1.5em]";

export const selectArrowStyle: React.CSSProperties = {
  backgroundImage: `url("data:image/svg+xml,%3csvg xmlns='http://www.w3.org/2000/svg' fill='none' viewBox='0 0 20 20'%3e%3cpath stroke='%236b7280' stroke-linecap='round' stroke-linejoin='round' stroke-width='1.5' d='M6 8l4 4 4-4'/%3e%3c/svg%3e")`,
  backgroundPosition: "right 0.5rem center",
  backgroundRepeat: "no-repeat",
  backgroundSize: "1.5em 1.5em",
  backgroundColor: "transparent",
};

/** Font-mono input (for API keys, codes) */
export const inputMono =
  "rounded-md border border-zinc-200 px-3 py-[var(--ui-input-py)] font-mono text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200";

// ── Button styles ───────────────────────────────────────────────────

/** Toolbar button (borderless, compact) — used for Model/Workspace selectors */
export const toolbarButton =
  "inline-flex items-center gap-1 rounded-lg px-2 py-[var(--ui-btn-compact-py)] text-xs transition-colors text-zinc-500 hover:bg-zinc-200 dark:hover:bg-zinc-700 hover:text-zinc-700 dark:hover:text-zinc-200";

/** Toolbar button active state */
export const toolbarButtonActive =
  "bg-zinc-200 dark:bg-zinc-700 text-zinc-900 dark:text-zinc-100";

/** Dialog action button (Cancel/Save) — fixed width */
export const dialogButton =
  "w-20 rounded-md px-3 py-[var(--ui-btn-py)] text-xs font-medium text-center";

/** Dialog primary action (Save) */
export const dialogButtonPrimary =
  "w-20 rounded-md bg-zinc-800 px-3 py-[var(--ui-btn-py)] text-xs font-medium text-center text-white hover:bg-zinc-700 disabled:opacity-50 dark:bg-zinc-700 dark:hover:bg-zinc-600";

/** Dialog secondary action (Cancel) */
export const dialogButtonSecondary =
  "w-20 rounded-md px-3 py-[var(--ui-btn-py)] text-xs font-medium text-center text-zinc-600 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-700";

// ── Test result styles ──────────────────────────────────────────────

/** Test result message (success/error) */
export const testResultBase =
  "rounded-md px-3 py-[var(--ui-btn-py)] text-xs truncate";

export const testResultSuccess =
  "bg-green-50 text-green-700 dark:bg-green-900/20 dark:text-green-400";

export const testResultError =
  "bg-red-50 text-red-700 dark:bg-red-900/20 dark:text-red-400";
