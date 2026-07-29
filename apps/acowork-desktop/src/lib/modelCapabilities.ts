//! Model capability resolution helpers.
//!
//! `setCurrentModel` (user clicks the model dropdown) and the MQTT
//! `model_confirmed` handler (Runtime confirms a model switch) both need
//! to derive the new model's `default_reasoning_effort` from the cached
//! `availableModels` list. Previously each call site had its own
//! `find + ?? null` one-liner; this module is the single place that
//! defines the lookup so the two paths can't drift.

import type { ModelEntry } from "./types";

/**
 * Resolve the `default_reasoning_effort` for `(model, provider)` from
 * the supplied `availableModels` list.
 *
 * Returns `null` when the model isn't in the list, or when the list
 * entry has no `default_reasoning_effort` set. Callers treat `null` as
 * "use the model's default UI behaviour" (currently: hide the
 * reasoning-effort toggle button).
 *
 * The lookup is exact-match on both `name` AND `provider` because the
 * same model name can exist under multiple providers (e.g.
 * `claude-3-5-sonnet` on both Anthropic and Bedrock).
 */
export function resolveDefaultReasoningEffort(
  models: readonly ModelEntry[],
  model: string,
  provider: string,
): string | null {
  const entry = models.find((m) => m.name === model && m.provider === provider);
  return entry?.default_reasoning_effort ?? null;
}