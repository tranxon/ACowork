/**
 * Centralized Gateway configuration.
 * All API calls should use getGatewayUrl() instead of hardcoding the URL.
 */

import { useSettingsStore } from "../stores/settingsStore";
import type { GatewayMode } from "./types";

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
  return "local";
}

/**
 * Check if the current Gateway mode is remote.
 */
export function isGatewayModeRemote(): boolean {
  return getGatewayMode() === "remote";
}
