//! Session config mapping — single source of truth for HTTP and MQTT.
//!
//! Both `fetchSessionConfig` (HTTP `GET /api/agents/{id}/sessions/{sid}/config`)
//! and the MQTT `session_config` handler (retained on
//! `acowork/agents/{id}/sessions/{sid}/config`) need to translate their
//! payload into a `Partial<SessionChatState>` patch. Previously each
//! caller did its own field-by-field mapping, and the two implementations
//! drifted — the HTTP path cleared `reasoning_effort` to `null` on
//! "no override" while the MQTT path didn't, which hid the ChatPanel
//! reasoning-effort toggle button for every model until the session was
//! reopened. This module fixes that by giving both callers one function
//! with explicit semantics for "absent / null / no override".

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
 * must rename `model_id` → `model`, `provider_id` → `provider` and convert
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
 * — TypeScript accepts it because the subset is a subset.
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
   * - `true`  → HTTP path. `fetchSessionConfig` runs on cold-load /
   *              session-switch. A null field means "session has no value",
   *              and we must wipe the previous session's value from the
   *              UI, otherwise stale config from the prior session leaks
   *              through until the next retained MQTT push.
   *
   * - `false` → MQTT path. The retained `session_config` envelope only
   *              carries fields the Runtime explicitly emitted. An absent
   *              field means "not in this payload", which must NOT clobber
   *              the existing UI value (think: a `reasoning_effort` change
   *              shouldn't blank the model).
   *
   * Note: this flag does NOT affect `reasoning_effort`, which always
   * follows the preserve-on-null rule (see below).
   */
  clearOnNull: boolean;
}

/**
 * Single source of truth for mapping a SessionConfig snapshot (HTTP) or
 * `session_config` envelope (MQTT) into a `Partial<SessionChatState>`
 * patch. Both `fetchSessionConfig` and the MQTT `session_config` handler
 * MUST call this function. Do not inline the field-by-field mapping at
 * either call site — that's how the two paths drifted and hid the
 * reasoning-effort toggle button.
 *
 * # `reasoning_effort` is special — preserve-on-null, always
 *
 * Regardless of `clearOnNull`, `reasoning_effort` is **never** cleared to
 * `null`. The ChatPanel renders the reasoning-effort toggle button only
 * when `currentReasoningEffort != null` (ChatPanel.tsx:1999). If this
 * mapper cleared `reasoning_effort` to `null` on every "absent" signal,
 * the button would disappear for every session whose backend hasn't
 * emitted an explicit `reasoning_effort` yet — which is **every** fresh
 * session, because the Runtime only populates `reasoning_effort` when
 * the user (or a model switch) explicitly sets it. The previous
 * behaviour (button hidden until next model switch / MQTT confirmation)
 * was the user-visible bug.
 *
 * The correct mental model:
 *   - `reasoning_effort: null` means "not configured yet"
 *   - `reasoning_effort: "auto" | "off" | ...` means "explicitly set"
 *   - Visibility of the toggle is gated on the model's capability
 *     (`ModelEntry.default_reasoning_effort` / `reasoning`), not on
 *     whether the backend has emitted an explicit value.
 */
export function sessionConfigToPatch(
  config: SessionConfigInput,
  options: SessionConfigPatchOptions,
): SessionConfigPatch {
  const patch: SessionConfigPatch = {};
  const { clearOnNull } = options;

  // ── model ────────────────────────────────────────────────────────
  if (typeof config.model === "string" && config.model) {
    patch.model = config.model;
  } else if (clearOnNull) {
    patch.model = null;
  }

  // ── provider ─────────────────────────────────────────────────────
  if (typeof config.provider === "string" && config.provider) {
    patch.provider = config.provider;
  } else if (clearOnNull) {
    patch.provider = null;
  }

  // ── temperature (NaN is the Runtime "no override" sentinel) ──────
  if (typeof config.temperature === "number" && !Number.isNaN(config.temperature)) {
    patch.temperature = config.temperature;
  } else if (clearOnNull) {
    patch.temperature = null;
  }

  // ── reasoning_effort (always preserve-on-null, see function doc) ─
  if (typeof config.reasoning_effort === "string" && config.reasoning_effort) {
    patch.reasoningEffort = config.reasoning_effort;
  }

  return patch;
}