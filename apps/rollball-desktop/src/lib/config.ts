/**
 * Centralized Gateway configuration.
 * All API calls should use getGatewayUrl() instead of hardcoding the URL.
 */

export const DEFAULT_GATEWAY_URL = "http://127.0.0.1:19876";

/**
 * Get the current Gateway URL.
 * Falls back to DEFAULT_GATEWAY_URL if no custom URL is configured.
 */
export function getGatewayUrl(): string {
  // For now, return the default. In the future, this can read from
  // settingsStore or environment variables.
  return DEFAULT_GATEWAY_URL;
}
