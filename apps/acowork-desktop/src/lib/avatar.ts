/**
 * Shared avatar helpers.
 *
 * - `resolveAgentAvatarUrl(agentId)` builds the full Gateway URL that serves
 *   the agent's packaged avatar (from manifest.avatar) as image bytes.
 *   Returns `null` if the URL cannot be built.
 *
 * - `normalizeBuiltinAvatarId(value)` parses a user-authored `builtin_avatar`
 *   value from manifest.toml — accepts either "icon-05" (canonical) or bare
 *   numeric forms "5" / "05" — and returns a canonical "icon-XX" string when
 *   the value matches a bundled icon. Returns `null` for unknown values so
 *   the caller can fall back to a random pick.
 *
 * - `pickRandomBuiltinIconId()` returns a uniformly-random builtin icon ID
 *   from `BUILTIN_ICON_IDS`. Returns `null` if no builtin icons are bundled
 *   (the caller should fall back to a non-icon renderer).
 *
 * - `pickDeterministicBuiltinIconId(seed)` returns a stable random builtin
 *   icon ID for a given string seed. Used to avoid a flash-of-gradient when
 *   an agent has no profile icon and no packaged avatar: the first render
 *   shows the same icon that the install hook will eventually persist.
 */

import { BUILTIN_ICONS, BUILTIN_ICON_IDS } from "./builtinIcons";
import { getGatewayUrl } from "./config";

/** Build the Gateway URL that serves the agent's packaged avatar (if any). */
export function resolveAgentAvatarUrl(agentId: string): string | null {
  if (!agentId) return null;
  try {
    return `${getGatewayUrl()}/api/agents/${encodeURIComponent(agentId)}/avatar`;
  } catch {
    return null;
  }
}

/**
 * Normalise a manifest `builtin_avatar` value to the canonical "icon-XX" form
 * and validate it against the bundled icon set. Returns the canonical ID
 * (e.g. "icon-05") when the value matches a bundled icon, otherwise `null`.
 *
 * Accepted inputs:
 * - "icon-05", "ICON-5" — canonical/case-insensitive form
 * - "5", "05"          — bare numeric form (1-99)
 *
 * Anything else (empty string, non-numeric, out of range, typo) returns
 * `null` so the caller can fall back to a random icon.
 */
export function normalizeBuiltinAvatarId(value: string | null | undefined): string | null {
  if (!value) return null;
  const trimmed = value.trim();
  if (!trimmed) return null;
  const lower = trimmed.toLowerCase();
  // Try "icon-NN" first (canonical form)
  if (lower.startsWith("icon-")) {
    const num = lower.slice("icon-".length);
    if (/^\d{1,2}$/.test(num)) {
      const candidate = `icon-${num.padStart(2, "0")}`;
      if (BUILTIN_ICONS[candidate]) return candidate;
    }
    return null;
  }
  // Then bare numeric 1-99
  if (/^\d{1,2}$/.test(lower)) {
    const candidate = `icon-${lower.padStart(2, "0")}`;
    if (BUILTIN_ICONS[candidate]) return candidate;
  }
  return null;
}

/** Pick a uniformly random builtin icon ID, or null if none are bundled. */
export function pickRandomBuiltinIconId(): string | null {
  if (BUILTIN_ICON_IDS.length === 0) return null;
  const idx = Math.floor(Math.random() * BUILTIN_ICON_IDS.length);
  return BUILTIN_ICON_IDS[idx] ?? null;
}

/** Pick a stable builtin icon ID for the given seed string. */
export function pickDeterministicBuiltinIconId(seed: string): string | null {
  if (BUILTIN_ICON_IDS.length === 0 || !seed) return null;
  let hash = 0;
  for (let i = 0; i < seed.length; i++) {
    hash = (seed.charCodeAt(i) + (hash << 5) - hash) | 0;
  }
  const idx = Math.abs(hash) % BUILTIN_ICON_IDS.length;
  return BUILTIN_ICON_IDS[idx] ?? null;
}
