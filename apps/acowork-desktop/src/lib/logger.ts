/**
 * Lightweight frontend logger with runtime level control.
 *
 * Design goals:
 * - O(1) level check (integer comparison) so disabled logs are essentially free
 * - No React re-renders: level is stored in a module-level variable, not a
 *   reactive store. Components that need to reflect the level (e.g. Settings)
 *   read from `settingsStore.frontendLogLevel` which IS reactive.
 * - Drop-in replacement for `console.*`: same variadic `unknown[]` args.
 *
 * Usage:
 *   import { log } from "@/lib/logger";
 *   log.debug("hello", data);        // instead of console.log(...)
 *   log.warn("something odd", err);  // instead of console.warn(...)
 *   log.error("failed", err);        // instead of console.error(...)
 *
 * Level hierarchy (lower number = more verbose):
 *   trace=0  debug=1  info=2  warn=3  error=4  off=5
 *
 * A message at level L is emitted only when `L >= currentLevel`.
 * Default level is "warn" so that debug/info noise is suppressed in production
 * while warnings and errors are always visible.
 */

export type LogLevel = "trace" | "debug" | "info" | "warn" | "error" | "off";

const LEVEL_VALUES: Record<LogLevel, number> = {
  trace: 0,
  debug: 1,
  info: 2,
  warn: 3,
  error: 4,
  off: 5,
};

const STORAGE_KEY = "acowork-frontend-log-level";

/** Read initial level from localStorage, fallback to "warn". */
function readInitialLevel(): number {
  try {
    const stored = localStorage.getItem(STORAGE_KEY);
    if (stored && stored in LEVEL_VALUES) {
      return LEVEL_VALUES[stored as LogLevel];
    }
  } catch {
    // localStorage unavailable
  }
  return LEVEL_VALUES.warn;
}

/** Module-level current level (numeric for O(1) comparison). */
let currentLevel = readInitialLevel();

/**
 * Update the logger level at runtime.
 * Called by `settingsStore.setFrontendLogLevel()` whenever the user
 * changes the dropdown in Settings, and once during store init.
 */
export function setLevel(level: LogLevel): void {
  currentLevel = LEVEL_VALUES[level] ?? LEVEL_VALUES.warn;
}

/** Get the current level as a string (for display purposes). */
export function getLevel(): LogLevel {
  return (
    (Object.keys(LEVEL_VALUES).find(
      (k) => LEVEL_VALUES[k as LogLevel] === currentLevel,
    ) as LogLevel) ?? "warn"
  );
}

/**
 * The logger object. Each method is a thin wrapper around the
 * corresponding `console.*` call, gated by an integer comparison.
 *
 * `log.debug` is the replacement for `console.log` (most existing
 * `console.log` calls are debug-level diagnostics, not user-facing info).
 */
export const log = {
  trace: (...args: unknown[]): void => {
    if (LEVEL_VALUES.trace >= currentLevel) console.log(...args);
  },
  debug: (...args: unknown[]): void => {
    if (LEVEL_VALUES.debug >= currentLevel) console.log(...args);
  },
  info: (...args: unknown[]): void => {
    if (LEVEL_VALUES.info >= currentLevel) console.info(...args);
  },
  warn: (...args: unknown[]): void => {
    if (LEVEL_VALUES.warn >= currentLevel) console.warn(...args);
  },
  error: (...args: unknown[]): void => {
    // error is always emitted unless level is "off"
    if (LEVEL_VALUES.error >= currentLevel) console.error(...args);
  },
} as const;
