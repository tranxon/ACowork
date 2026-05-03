/**
 * Global UI style constants for consistent component appearance.
 * All input fields, buttons, and interactive elements should use these tokens.
 */

// ── Input field styles ──────────────────────────────────────────────

/** Standard input field (text, number, etc.) */
export const inputBase =
  "rounded-md border border-zinc-200 px-3 py-2 text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200";

/** Read-only input field */
export const inputReadonly =
  "rounded-md border border-zinc-200 bg-zinc-50 px-3 py-2 text-xs dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-400";

/** Select/dropdown field */
export const selectBase =
  "rounded-md border border-zinc-200 bg-white px-3 py-2 pr-8 text-xs appearance-none dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200";

/** Font-mono input (for API keys, codes) */
export const inputMono =
  "rounded-md border border-zinc-200 px-3 py-2 font-mono text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200";

// ── Button styles ───────────────────────────────────────────────────

/** Toolbar button (borderless, compact) — used for Model/Workspace selectors */
export const toolbarButton =
  "inline-flex items-center gap-1 rounded-lg px-2 py-1 text-xs transition-colors text-zinc-500 hover:bg-zinc-200 dark:hover:bg-zinc-700 hover:text-zinc-700 dark:hover:text-zinc-200";

/** Toolbar button active state */
export const toolbarButtonActive =
  "bg-zinc-200 dark:bg-zinc-700 text-zinc-900 dark:text-zinc-100";

/** Dialog action button (Cancel/Save) — fixed width */
export const dialogButton =
  "w-20 rounded-md px-3 py-1.5 text-xs font-medium text-center";

/** Dialog primary action (Save) */
export const dialogButtonPrimary =
  "w-20 rounded-md bg-zinc-800 px-3 py-1.5 text-xs font-medium text-center text-white hover:bg-zinc-700 disabled:opacity-50 dark:bg-zinc-700 dark:hover:bg-zinc-600";

/** Dialog secondary action (Cancel) */
export const dialogButtonSecondary =
  "w-20 rounded-md px-3 py-1.5 text-xs font-medium text-center text-zinc-600 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-700";

// ── Test result styles ──────────────────────────────────────────────

/** Test result message (success/error) */
export const testResultBase =
  "rounded-md px-3 py-1.5 text-xs truncate";

export const testResultSuccess =
  "bg-green-50 text-green-700 dark:bg-green-900/20 dark:text-green-400";

export const testResultError =
  "bg-red-50 text-red-700 dark:bg-red-900/20 dark:text-red-400";
