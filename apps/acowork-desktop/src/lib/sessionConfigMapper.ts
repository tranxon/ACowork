//! Session config mapping - single source of truth for HTTP and MQTT.
//!
//! Both `fetchSessionConfig` (HTTP `GET /api/agents/{id}/sessions/{sid}/config`)
//! and the MQTT `session_config` handler (retained on
//! `acowork/agents/{id}/sessions/{sid}/config`) need to translate their
//! payload into a `Partial<SessionChatState>` patch. This module is the
//! single mapping function used by both call sites.
//!
//! # Design principle: backend is the source of truth
//!
//! The backend resolves `reasoning_effort` through a three-level priority
//! chain (persisted -> provider default -> supports_reasoning -> Auto ->
//! None) and always publishes the effective value. The frontend must use
//! the backend value directly - no preserve-on-null, no caching, no state
//! stitching. This is the only way to guarantee the UI reflects the true
//! backend state.
//!
//! Both HTTP and MQTT paths use `clearOnNull: true` because both deliver
//! a full snapshot of the session config (not a partial delta). An
//! absent/null field means "the session has no value for this field" and
//! must overwrite any stale value from a previous session or model.

/**
 * Unified session config input shape used by both HTTP and MQTT paths.
 *
 * HTTP `SessionConfigSnapshot` (`SessionConfigSnapshot`, see ADR-047):
 *   - fields are `string | number | null | undefined`
 *   - `null` means "not set / no override"
 *
 * MQTT `SessionConfig` envelope (see `mqtt_payload.proto`):
 *   - field names are `model_id` / `provider_id` (not `model` / `provider`)
 *   - prost can't encode `Option<T>`, so the wire-format sentinel for
 *     "no override" is `""` for strings and `NaN` for floats
 *
 * The HTTP caller passes the deserialized JSON as-is. The MQTT caller
 * must rename `model_id` -> `model`, `provider_id` -> `provider` and convert
 * `""` to `null` / `NaN` to `null` before calling.
 */
export interface SessionConfigInput {
  model?: string | null;
  provider?: string | null;
  reasoning_effort?: string | null;
  temperature?: number | null;
}

/**
 * Subset of `SessionChatState` that this mapper can produce.
 *
 * Declared locally (not imported from `chatStore.ts`) so the mapper
 * stays decoupled from `SessionChatState`'s ~30 unrelated fields
 * (`messages`, `tokenUsage`, `isAssistantReplying`, ...). The patch
 * shape is structurally compatible with `Partial<SessionChatState>`
 * - TypeScript accepts it because the subset is a subset.
 */
export interface SessionConfigPatch {
  model?: string | null;
  provider?: string | null;
  reasoningEffort?: string | null;
  temperature?: number | null;
}

export interface SessionConfigPatchOptions {
  /**
   * Whether absent / null values should be propagated as explicit `null`
   * in the patch (true), or simply left out of the patch (false).
   *
   * Both HTTP and MQTT paths should use `true` because both deliver a
   * full snapshot. A null field means "session has no value" and must
   * clear any stale value from the UI.
   */
  clearOnNull: boolean;
}

/**
 * Single source of truth for mapping a SessionConfig snapshot (HTTP) or
 * `session_config` envelope (MQTT) into a `Partial<SessionChatState>`
 * patch. Both `fetchSessionConfig` and the MQTT `session_config` handler
 * MUST call this function.
 *
 * The backend is the source of truth - the frontend uses the backend
 * value directly, with no preserve-on-null or state stitching.
 */
export function sessionConfigToPatch(
  config: SessionConfigInput,
  options: SessionConfigPatchOptions,
): SessionConfigPatch {
  const patch: SessionConfigPatch = {};
  const { clearOnNull } = options;

  // -- model --
  if (typeof config.model === "string" && config.model) {
    patch.model = config.model;
  } else if (clearOnNull) {
    patch.model = null;
  }

  // -- provider --
  if (typeof config.provider === "string" && config.provider) {
    patch.provider = config.provider;
  } else if (clearOnNull) {
    patch.provider = null;
  }

  // -- temperature (NaN is the Runtime "no override" sentinel) --
  if (typeof config.temperature === "number" && !Number.isNaN(config.temperature)) {
    patch.temperature = config.temperature;
  } else if (clearOnNull) {
    patch.temperature = null;
  }

  // -- reasoning_effort (same logic as all other fields) --
  if (typeof config.reasoning_effort === "string" && config.reasoning_effort) {
    patch.reasoningEffort = config.reasoning_effort;
  } else if (clearOnNull) {
    patch.reasoningEffort = null;
  }

  return patch;
}
