//! Gateway HTTP API client for models.dev integration

import type {
  ProviderModelsResponse,
  ProviderListEntry,
  ModelInfo,
  BackendUserProfile,
  UserProfileListResponse,
  CreateUserRequest,
  UpdateUserRequest,
  EmbeddingModelsResponse,
  EmbeddingModelActionResponse,
  EmbeddingModelStatusResponse,
  EmbeddingTestResponse,
  MigrationProgressResponse,
  SelectModelMigrationResponse,
  LspEndpointResponse,
  LspInstallScriptResponse,
  LspInstallRunResponse,
  LspServerStatusEntry,
  LspServersWithStatus,
} from "./types";
import { getGatewayUrl } from "./config";

// ── LSP Relay endpoint cache ───────────────────────────────────────────
//
// The relay endpoint is queried once and cached. On error or invalidation,
// the cache is cleared so the next call re-fetches.

let relayEndpointCache: Promise<LspEndpointResponse | null> | null = null;

/**
 * Get the cached LSP Relay endpoint, fetching from Gateway if needed.
 *
 * Returns `null` when the relay is not available (not running or not ready).
 * On fetch error, the cache is cleared so the next call retries.
 */
export async function getCachedLspRelayEndpoint(
  gatewayUrl = getGatewayUrl(),
): Promise<LspEndpointResponse | null> {
  if (!relayEndpointCache) {
    relayEndpointCache = fetchLspEndpoint(gatewayUrl)
      .then((ep) => (ep.available && ep.port != null ? ep : null))
      .catch((err) => {
        relayEndpointCache = null; // Clear cache on error
        throw err;
      });
  }
  return relayEndpointCache;
}

/** Invalidate the cached LSP Relay endpoint (e.g. after connection failure). */
export function invalidateLspRelayEndpointCache(): void {
  relayEndpointCache = null;
}

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

/** Discover models from a custom provider's base URL (OpenAI-compatible /v1/models) */
export async function discoverModels(
  baseUrl: string,
  apiKey?: string,
  gatewayUrl = getGatewayUrl(),
): Promise<ModelInfo[]> {
  const resp = await fetch(`${gatewayUrl}/api/models/discover`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ base_url: baseUrl, api_key: apiKey || undefined }),
  });
  if (!resp.ok) {
    const err = await resp.json().catch(() => ({ error: resp.statusText }));
    throw new Error((err as { error?: string }).error ?? `Discover failed: ${resp.status}`);
  }
  const data = await resp.json();
  return data.models ?? [];
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

/** Reset onboarding and trigger Gateway models cache reload.
 *
 *  The frontend onboarding flag is always cleared first — the user's
 *  intent is to reset the local wizard. The Gateway-side reset is
 *  best-effort: if the remote Gateway is unreachable (e.g. WSL IP drift,
 *  firewall, Gateway process not running), the wizard still reappears
 *  on reload. A previous version put `removeItem` after `await`, which
 *  silently failed to reset the UI whenever the Gateway call threw.
 */
export async function resetOnboarding(
  gatewayUrl = getGatewayUrl(),
): Promise<{ status: string; source: string }> {
  localStorage.removeItem("acowork_onboarding");
  try {
    return await resetGateway(gatewayUrl);
  } catch (e) {
    console.warn(
      "Gateway reset failed (frontend onboarding state cleared anyway):",
      e,
    );
    return { status: "frontend_only", source: "local" };
  }
}

// ── Embedding Model API ──────────────────────────────────────────────────

/** Fetch all embedding models with status */
export async function fetchEmbeddingModels(
  gatewayUrl = getGatewayUrl(),
): Promise<EmbeddingModelsResponse> {
  const resp = await fetch(`${gatewayUrl}/api/embedding-models`);
  if (!resp.ok) throw new Error(`Failed to fetch embedding models: ${resp.status}`);
  return resp.json();
}

/** Trigger download of an embedding model */
export async function downloadEmbeddingModel(
  modelId: string,
  variant?: string,
  gatewayUrl = getGatewayUrl(),
): Promise<EmbeddingModelActionResponse> {
  const resp = await fetch(`${gatewayUrl}/api/embedding-models/${modelId}/download`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ variant: variant ?? null }),
  });
  const data = await resp.json();
  if (!resp.ok) {
    throw new Error((data as EmbeddingModelActionResponse).message ?? `Download failed: ${resp.status}`);
  }
  return data as EmbeddingModelActionResponse;
}

/** Select (activate) an embedding model */
export async function selectEmbeddingModel(
  modelId: string,
  force = false,
  gatewayUrl = getGatewayUrl(),
): Promise<EmbeddingModelActionResponse> {
  const resp = await fetch(`${gatewayUrl}/api/embedding-models/${modelId}/select`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ force }),
  });
  const data = await resp.json();
  if (!resp.ok) {
    const actionResp = data as EmbeddingModelActionResponse;
    // Return the response even on CONFLICT so caller can handle dimension_mismatch
    if (resp.status === 409) return actionResp;
    throw new Error(actionResp.message ?? `Select failed: ${resp.status}`);
  }
  return data as EmbeddingModelActionResponse;
}

/** Poll download progress for an embedding model */
export async function fetchEmbeddingModelStatus(
  modelId: string,
  gatewayUrl = getGatewayUrl(),
): Promise<EmbeddingModelStatusResponse> {
  const resp = await fetch(`${gatewayUrl}/api/embedding-models/${modelId}/status`);
  if (!resp.ok) throw new Error(`Failed to fetch status: ${resp.status}`);
  return resp.json();
}

/** Delete a downloaded embedding model's files */
export async function deleteEmbeddingModel(
  modelId: string,
  gatewayUrl = getGatewayUrl(),
): Promise<EmbeddingModelActionResponse> {
  const resp = await fetch(`${gatewayUrl}/api/embedding-models/${modelId}`, {
    method: "DELETE",
  });
  const data = await resp.json();
  if (!resp.ok) {
    throw new Error((data as EmbeddingModelActionResponse).message ?? `Delete failed: ${resp.status}`);
  }
  return data as EmbeddingModelActionResponse;
}

/** Test the currently loaded embedding model */
export async function testEmbeddingModel(
  gatewayUrl = getGatewayUrl(),
): Promise<EmbeddingTestResponse> {
  const resp = await fetch(`${gatewayUrl}/api/embedding-models/test`, {
    method: "POST",
  });
  if (!resp.ok) throw new Error(`Test request failed: ${resp.status}`);
  return resp.json();
}

/** Start embedding dimension migration for agents */
export async function startMigration(
  modelId: string,
  agentIds: string[],
  gatewayUrl = getGatewayUrl(),
): Promise<EmbeddingModelActionResponse> {
  const resp = await fetch(`${gatewayUrl}/api/embedding-models/${modelId}/start-migration`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ agent_ids: agentIds }),
  });
  const data = await resp.json();
  if (!resp.ok) throw new Error((data as EmbeddingModelActionResponse).message ?? `Migration start failed: ${resp.status}`);
  return data as EmbeddingModelActionResponse;
}

/** Get embedding migration progress for all agents */
export async function fetchMigrationProgress(
  gatewayUrl = getGatewayUrl(),
): Promise<MigrationProgressResponse> {
  const resp = await fetch(`${gatewayUrl}/api/embedding-models/migration-progress`);
  if (!resp.ok) throw new Error(`Failed to fetch migration progress: ${resp.status}`);
  return resp.json();
}

/** Select embedding model and return full migration response (handles 200 with migration info) */
export async function selectEmbeddingModelWithMigration(
  modelId: string,
  force: boolean,
  gatewayUrl = getGatewayUrl(),
): Promise<SelectModelMigrationResponse | EmbeddingModelActionResponse> {
  const resp = await fetch(`${gatewayUrl}/api/embedding-models/${modelId}/select`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ force }),
  });
  const data = await resp.json();
  if (!resp.ok) throw new Error((data as EmbeddingModelActionResponse).message ?? `Select failed: ${resp.status}`);
  return data as SelectModelMigrationResponse | EmbeddingModelActionResponse;
}


// ── LSP API ──────────────────────────────────────────────────────────────

/**
 * Fetch the LSP Relay endpoint from the Gateway.
 *
 * The Gateway manages the LSP Relay process and exposes its address via
 * `GET /api/lsp/endpoint`. Desktop App and Agent Runtime use this to
 * discover the relay, then connect directly.
 *
 * Returns `{ available: false, port: null }` when the relay is not running.
 */
export async function fetchLspEndpoint(
  gatewayUrl = getGatewayUrl(),
): Promise<LspEndpointResponse> {
  const resp = await fetch(`${gatewayUrl}/api/lsp/endpoint`);
  if (!resp.ok) throw new Error(`Failed to fetch LSP endpoint: ${resp.status}`);
  return resp.json();
}

/**
 * Build the base HTTP URL for the LSP Relay.
 *
 * Returns `null` if the relay is not available.
 */
export async function getLspRelayUrl(
  gatewayUrl = getGatewayUrl(),
): Promise<string | null> {
  const ep = await fetchLspEndpoint(gatewayUrl);
  if (!ep.available || ep.port == null) return null;
  return `http://${ep.host}:${ep.port}`;
}

// ── LSP Relay direct API (servers / status / install) ───────────────────
//
// These functions call the LSP Relay directly (not through the Gateway).
// The caller must provide the relay base URL, typically obtained via
// `getLspRelayUrl()`.

/**
 * Fetch configured LSP servers together with per-language install status
 * in a single round-trip.
 *
 * This is the preferred initial-load / Refresh call: the UI gets the
 * server list and the install badges atomically, so it never renders
 * a row whose status has not yet been resolved. The backend runs the
 * PATH probes with bounded concurrency (4 in flight), keeping total
 * wall time roughly bounded by a single probe timeout (~2s worst case).
 */
export async function fetchLspServersWithStatus(
  relayUrl: string,
): Promise<LspServersWithStatus> {
  const resp = await fetch(`${relayUrl}/api/lsp/servers-with-status`);
  if (!resp.ok) {
    throw new Error(
      `Failed to fetch LSP servers with status: ${resp.status}`,
    );
  }
  return resp.json();
}

/**
 * Re-probe per-language LSP installation status from the LSP Relay.
 *
 * Used by the per-row Check button: the user has already seen the
 * list, so we only need to re-probe status — there's no need to
 * re-fetch the server config.
 *
 * The relay probes `PATH` for each configured candidate command and
 * returns whether a usable binary was found. This is the source of
 * truth for the UI's "installed" badge and is used to disable the
 * Install button for already-installed servers.
 *
 * Unlike `handleCheck` in `LspTab`, this endpoint does NOT spawn any
 * LSP process — it's a fast PATH lookup, so it's safe to call on mount.
 */
export async function fetchLspStatus(
  relayUrl: string,
): Promise<LspServerStatusEntry[]> {
  const resp = await fetch(`${relayUrl}/api/lsp/status`);
  if (!resp.ok) throw new Error(`Failed to fetch LSP status: ${resp.status}`);
  return resp.json();
}

/** Fetch install script content for a language from the LSP Relay */
export async function fetchLspInstallScript(
  language: string,
  relayUrl: string,
): Promise<LspInstallScriptResponse> {
  const resp = await fetch(`${relayUrl}/api/lsp/install/${encodeURIComponent(language)}`);
  if (!resp.ok) throw new Error(`Failed to fetch install script: ${resp.status}`);
  return resp.json();
}

/** Run the install script for a language on the LSP Relay */
export async function runLspInstall(
  language: string,
  relayUrl: string,
): Promise<LspInstallRunResponse> {
  const resp = await fetch(`${relayUrl}/api/lsp/install/${encodeURIComponent(language)}`, {
    method: "POST",
  });
  const data = await resp.json();
  if (!resp.ok) throw new Error((data as { error?: string }).error ?? `Install failed: ${resp.status}`);
  return data as LspInstallRunResponse;
}


