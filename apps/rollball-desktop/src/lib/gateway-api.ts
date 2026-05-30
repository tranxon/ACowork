//! Gateway HTTP API client for models.dev integration

import type {
  ProviderModelsResponse,
  ProviderListEntry,
  BackendUserProfile,
  UserProfileListResponse,
  CreateUserRequest,
  UpdateUserRequest,
} from "./types";
import { getGatewayUrl } from "./config";

/** Fetch all providers from Gateway's models cache */
export async function fetchProviders(
  gatewayUrl = getGatewayUrl(),
): Promise<ProviderListEntry[]> {
  const resp = await fetch(`${gatewayUrl}/api/models`);
  if (!resp.ok) throw new Error(`Failed to fetch providers: ${resp.status}`);
  const data = await resp.json();
  return data.providers as ProviderListEntry[];
}

/** Fetch models for a specific provider from Gateway's models cache */
export async function fetchProviderModels(
  providerId: string,
  gatewayUrl = getGatewayUrl(),
): Promise<ProviderModelsResponse> {
  const resp = await fetch(`${gatewayUrl}/api/models/${providerId}`);
  if (!resp.ok)
    throw new Error(`Failed to fetch models for ${providerId}: ${resp.status}`);
  return resp.json();
}

// ── User Profile API ────────────────────────────────────────────────────

/** Fetch all user profiles from Gateway */
export async function fetchUsers(
  gatewayUrl = getGatewayUrl(),
): Promise<UserProfileListResponse> {
  const resp = await fetch(`${gatewayUrl}/api/users`);
  if (!resp.ok) throw new Error(`Failed to fetch users: ${resp.status}`);
  return resp.json();
}

/** Get the currently active user profile */
export async function fetchActiveUser(
  gatewayUrl = getGatewayUrl(),
): Promise<BackendUserProfile | null> {
  const data = await fetchUsers(gatewayUrl);
  return data.users.find((u) => u.is_active) ?? null;
}

/** Create a new user profile */
export async function createUser(
  profile: CreateUserRequest,
  gatewayUrl = getGatewayUrl(),
): Promise<BackendUserProfile> {
  const resp = await fetch(`${gatewayUrl}/api/users`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(profile),
  });
  if (!resp.ok) {
    const err = await resp.json().catch(() => ({ error: resp.statusText }));
    throw new Error((err as { error?: string }).error ?? `Failed to create user: ${resp.status}`);
  }
  return resp.json();
}

/** Update an existing user profile */
export async function updateUser(
  userId: string,
  profile: UpdateUserRequest,
  gatewayUrl = getGatewayUrl(),
): Promise<BackendUserProfile> {
  const resp = await fetch(`${gatewayUrl}/api/users/${userId}`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(profile),
  });
  if (!resp.ok) {
    const err = await resp.json().catch(() => ({ error: resp.statusText }));
    throw new Error((err as { error?: string }).error ?? `Failed to update user: ${resp.status}`);
  }
  return resp.json();
}

/** Activate a user (deactivates all others) */
export async function activateUser(
  userId: string,
  gatewayUrl = getGatewayUrl(),
): Promise<BackendUserProfile> {
  const resp = await fetch(`${gatewayUrl}/api/users/${userId}/activate`, {
    method: "POST",
  });
  if (!resp.ok) {
    const err = await resp.json().catch(() => ({ error: resp.statusText }));
    throw new Error((err as { error?: string }).error ?? `Failed to activate user: ${resp.status}`);
  }
  return resp.json();
}

/** Reset Gateway state (reload models cache from disk or background fetch) */
export async function resetGateway(
  gatewayUrl = getGatewayUrl(),
): Promise<{ status: string; source: string }> {
  const resp = await fetch(`${gatewayUrl}/api/gateway/reset`, {
    method: "POST",
  });
  if (!resp.ok) throw new Error(`Failed to reset Gateway: ${resp.status}`);
  return resp.json();
}

/** Reset onboarding and trigger Gateway models cache reload */
export async function resetOnboarding(
  gatewayUrl = getGatewayUrl(),
): Promise<{ status: string; source: string }> {
  const result = await resetGateway(gatewayUrl);
  // Clear frontend onboarding state
  localStorage.removeItem("rollball_onboarding");
  return result;
}
