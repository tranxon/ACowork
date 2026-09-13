/**
 * Centralized Gateway configuration helpers.
 *
 * Default values are defined in `./defaults.ts` (a pure-constants module with
 * no runtime imports) and re-exported here for backward compatibility with
 * existing callers that import `DEFAULT_*` from `./config`.
 */

import { useSettingsStore } from "../stores/settingsStore";
import {
  DEFAULT_GATEWAY_URL,
  DEFAULT_GATEWAY_MODE,
} from "./defaults";
import type { GatewayMode } from "./types";

export {
  DEFAULT_GATEWAY_URL,
  DEFAULT_GATEWAY_MODE,
  DEFAULT_THEME,
  DEFAULT_FONT_SIZE,
  DEFAULT_LOG_LEVEL,
  DEFAULT_CONTENT_WIDTH,
  DEFAULT_OPACITY,
  DEFAULT_ACCENT_COLOR,
  DEFAULT_LOG_FILE_SIZE_MB,
  DEFAULT_LOG_FILE_COUNT,
} from "./defaults";

/**
 * Get the current Gateway URL.
 * Reads from settingsStore if available (user-configured), falls back to DEFAULT_GATEWAY_URL.
 * Supports remote Desktop ↔ Gateway scenarios.
 */
export function getGatewayUrl(): string {
  try {
    const url = useSettingsStore.getState().gatewayUrl;
    if (url) return url;
  } catch {
    // settingsStore not yet available (e.g. SSR), fall through to default
  }
  return DEFAULT_GATEWAY_URL;
}

/**
 * Check if the current Gateway URL points to a local address.
 *
 * Used by local-only features (e.g. "Reveal in File Explorer" — opening
 * the OS file manager on a remote Gateway host has no value to the user).
 * Debug Protocol itself is NOT local-only — RPC rides Gateway HTTP and
 * events ride MQTT with the broker host derived from the Gateway URL
 * (ADR-048 D6 + ADR-055/058), so remote mode is fully supported.
 */
export function isGatewayLocal(): boolean {
  const url = getGatewayUrl();
  try {
    const hostname = new URL(url).hostname;
    return hostname === "localhost" || hostname === "127.0.0.1" || hostname === "[::1]";
  } catch {
    // URL unparseable (e.g. missing protocol) — try manual hostname extraction
    const hostname = url.replace(/^https?:\/\//i, '').split('/')[0].split(':')[0];
    return hostname === "localhost" || hostname === "127.0.0.1" || hostname === "[::1]";
  }
}

/**
 * Get the current Gateway deployment mode.
 * Reads from settingsStore, defaults to "local".
 */
export function getGatewayMode(): GatewayMode {
  try {
    const mode = useSettingsStore.getState().gatewayMode;
    if (mode === "local" || mode === "remote") return mode;
  } catch {
    // settingsStore not yet available
  }
  return DEFAULT_GATEWAY_MODE;
}

/**
 * Check if the current Gateway mode is remote.
 */
export function isGatewayModeRemote(): boolean {
  return getGatewayMode() === "remote";
}
