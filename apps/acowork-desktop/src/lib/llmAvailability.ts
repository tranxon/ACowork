//! Three-state LLM availability, mirrored from the `SessionConfig.llm_availability`
//! retained MQTT topic (see `core/acowork-core/proto/mqtt_payload.proto`).
//!
//! Wire enum (prost-generated `LlmAvailability`):
//! - `0` UNSPECIFIED — runtime hasn't published yet, never render a banner
//! - `1` LOADING     — bootstrap not READY / vault not populated, render placeholder
//! - `2` CONFIGURED  — vault has at least one usable provider, render nothing
//! - `3` MISSING     — vault empty / no usable provider, render red banner

/**
 * Frontend projection of the wire enum. Strings (not numbers) keep the
 * call sites readable and survive any future wire re-numbering.
 *
 * `unspecified` (rather than `loading`) is the initial state so the
 * Desktop renders NO banner before the first retained message arrives —
 * the previous boolean check caused the visible red flash on startup.
 */
export type LlmAvailability =
  | "unspecified"
  | "loading"
  | "configured"
  | "missing";

/**
 * Map a raw integer from the protobuf envelope to the frontend projection.
 *
 * Any out-of-range value (a runtime ahead of the Desktop with a new variant,
 * or `null`/`undefined` from a stale payload) collapses to `"unspecified"`
 * — the safe default that hides the banner rather than flashing it.
 */
export function llmAvailabilityFromWire(raw: unknown): LlmAvailability {
  switch (raw) {
    case 0:
    case "LLM_AVAILABILITY_UNSPECIFIED":
      return "unspecified";
    case 1:
    case "LLM_AVAILABILITY_LOADING":
      return "loading";
    case 2:
    case "LLM_AVAILABILITY_CONFIGURED":
      return "configured";
    case 3:
    case "LLM_AVAILABILITY_MISSING":
      return "missing";
    default:
      return "unspecified";
  }
}