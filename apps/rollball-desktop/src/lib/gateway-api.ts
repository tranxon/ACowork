//! Gateway HTTP API client for models.dev integration

import type { ProviderModelsResponse, ProviderListEntry } from "./types";
import { DEFAULT_GATEWAY_URL } from "./config";

/** Fetch all providers from Gateway's models cache */
export async function fetchProviders(
  gatewayUrl = DEFAULT_GATEWAY_URL,
): Promise<ProviderListEntry[]> {
  const resp = await fetch(`${gatewayUrl}/api/models`);
  if (!resp.ok) throw new Error(`Failed to fetch providers: ${resp.status}`);
  const data = await resp.json();
  return data.providers as ProviderListEntry[];
}

/** Fetch models for a specific provider from Gateway's models cache */
export async function fetchProviderModels(
  providerId: string,
  gatewayUrl = DEFAULT_GATEWAY_URL,
): Promise<ProviderModelsResponse> {
  const resp = await fetch(`${gatewayUrl}/api/models/${providerId}`);
  if (!resp.ok)
    throw new Error(`Failed to fetch models for ${providerId}: ${resp.status}`);
  return resp.json();
}
