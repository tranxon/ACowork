/**
 * Single source of truth for the auto-sleep (idle) timeout presets offered
 * in the Agent Setup panel.
 *
 * Why this file exists
 * --------------------
 * Previously the preset values (1800 / 3600 / 10800 / 0) were hardcoded in
 * two places inside `AgentSetupTab.tsx` (the <option> list AND the
 * `value=` match chain), so adjusting the options meant editing two spots
 * in the same component plus five locale files. With the presets declared
 * here once, `AgentSetupTab` renders both the options and the current-value
 * match from this array — changing the set of choices is a one-line edit
 * (plus adding/removing the corresponding `agentSetup.idleTimeoutOption*`
 * translation keys, which is unavoidable for i18n).
 *
 * Runtime contract
 * ----------------
 * The Runtime (`acowork-runtime/src/agent/idle_watcher.rs`) accepts ANY
 * `u64` timeout — the presets below are a UI-level constraint, not a
 * runtime one. Keep `DEFAULT_IDLE_TIMEOUT_SECS` here in sync with
 * `DEFAULT_IDLE_TIMEOUT_SECS` in `idle_watcher.rs` (1800 s = 30 min).
 */

export interface IdleTimeoutOption {
  /** Timeout in seconds. `0` = never sleep (keep process alive). */
  value: number;
  /** i18n key under the `agentSetup.` namespace. */
  labelKey: string;
}

/** Preset choices shown in the Agent Setup "Idle (auto-sleep) Timeout" select. */
export const IDLE_TIMEOUT_OPTIONS: IdleTimeoutOption[] = [
  { value: 1800, labelKey: "agentSetup.idleTimeoutOption30Min" },
  { value: 3600, labelKey: "agentSetup.idleTimeoutOption1Hour" },
  { value: 10800, labelKey: "agentSetup.idleTimeoutOption3Hour" },
  { value: 18000, labelKey: "agentSetup.idleTimeoutOption5Hour" },
  { value: 0, labelKey: "agentSetup.idleTimeoutOptionNever" },
];

/**
 * Default when the user has never chosen a value. Must match
 * `DEFAULT_IDLE_TIMEOUT_SECS` in
 * `core/acowork-runtime/src/agent/idle_watcher.rs` (1800 s = 30 min).
 */
export const DEFAULT_IDLE_TIMEOUT_SECS = 1800;

/**
 * Map a stored timeout (seconds) to the <select> `value` string.
 *
 * Returns `""` when the stored value is `undefined` or not one of the
 * current presets (e.g. a legacy 300/900 from before this file existed).
 * The component then shows the placeholder option instead of silently
 * mislabeling the value as a preset it isn't — that mislabeling was the
 * root cause of the "configured 30 min but actually 5 min" bug.
 */
export function idleTimeoutDisplayValue(secs: number | undefined): string {
  if (secs === undefined) return "";
  const preset = IDLE_TIMEOUT_OPTIONS.find((o) => o.value === secs);
  return preset ? String(preset.value) : "";
}
