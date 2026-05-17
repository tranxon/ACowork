/**
 * Centralized Gateway configuration.
 * All API calls should use getGatewayUrl() instead of hardcoding the URL.
 */

import { useSettingsStore } from "../stores/settingsStore";

export const DEFAULT_GATEWAY_URL = "http://127.0.0.1:19876";

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
 * Debug WebSocket is a direct Desktop ↔ Runtime connection only works locally.
 * In remote mode, the Debug Panel should skip the WebSocket connection.
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
