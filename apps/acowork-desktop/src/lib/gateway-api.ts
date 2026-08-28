//! Gateway HTTP API client for models.dev integration

import type {
  ProviderModelsResponse,
  ProviderListEntry,
  ModelInfo,
  BackendUserProfile,
  UserProfileListResponse,
  UserProfileMutationResponse,
  ActivateUserResponse,
  CreateUserRequest,
  UpdateUserRequest,
  OperationAck,
  EmbeddingModelsResponse,
  EmbeddingModelActionResponse,
  EmbeddingModelStatusResponse,
  EmbeddingTestResponse,
  MigrationProgressResponse,
  SelectModelMigrationResponse,
  AgentLspEndpointResponse,
  LspInstallScriptResponse,
  LspInstallRunResponse,
  LspServerStatusEntry,
  LspServersConfig,
  LspServersWithStatus,
  CompactModelRef,
  DefaultCompactModelResponse,
  NodeInfo,
} from "./types";
import { getGatewayUrl } from "./config";

// ── LSP Relay endpoint cache ───────────────────────────────────────────
//
// ADR-055 §6.7 (Phase 4): the relay is a node-local sidecar, so its
// endpoint is resolved PER AGENT via `GET /api/agents/{id}/lsp-endpoint`
// (the node hosting the agent runs the relay). Results are cached per
// agent. On error or invalidation, the cache entry is cleared so the
// next call re-fetches.

const relayEndpointCache = new Map<string, Promise<string | null>>();

/**
 * Get the cached LSP Relay base URL for an agent, fetching from Gateway
 * if needed.
 *
 * Returns `null` when the relay is not available (the hosting node has
 * not published a ready LSP relay state). On fetch error, the cache
 * entry is cleared so the next call retries.
 */
export async function getCachedLspRelayEndpoint(
  agentId: string,
  gatewayUrl = getGatewayUrl(),
): Promise<string | null> {
  let cached = relayEndpointCache.get(agentId);
  if (!cached) {
    cached = fetchAgentLspEndpoint(agentId, gatewayUrl)
      .then((ep) => (ep.ready && ep.endpoint ? ep.endpoint : null))
      .catch((err) => {
        relayEndpointCache.delete(agentId); // Clear cache on error
        throw err;
      });
    relayEndpointCache.set(agentId, cached);
  }
  return cached;
}

/**
 * Invalidate the cached LSP Relay endpoint for an agent, or for every
 * agent when called without an argument (e.g. after connection failure).
 */
export function invalidateLspRelayEndpointCache(agentId?: string): void {
  if (agentId !== undefined) {
    relayEndpointCache.delete(agentId);
  } else {
    relayEndpointCache.clear();
  }
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

/**
 * Create a new user profile.
 *
 * ADR-059 §7.3: the Gateway now answers `POST /api/users` with an
 * [`OperationAck`] (operation_id / state / resource_version /
 * terminal_error) — *not* the legacy `{ user, version }` envelope.
 * Older Desktop builds cast the response to `UserProfileMutationResponse`
 * and returned `data.user`, which was always `undefined` since
 * `OperationAck` has no `user` field; the call then silently
 * succeeded with the user profile never being threaded back to the
 * caller. OnboardingFlow treats this as fire-and-forget so the bug
 * never surfaced in the UI, but the contract was wrong.
 *
 * `updateUser` and `activateUser` still answer with their original
 * envelopes (`UserResponse` / `ActivateResponse`) — only the *create*
 * path is in the ADR-059 §7.3 set.
 */
export async function createUser(
  profile: CreateUserRequest,
  gatewayUrl = getGatewayUrl(),
): Promise<OperationAck> {
  const resp = await fetch(`${gatewayUrl}/api/users`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(profile),
  });
  if (!resp.ok) {
    const err = await resp.json().catch(() => ({ error: resp.statusText }));
    throw new Error((err as { error?: string }).error ?? `Failed to create user: ${resp.status}`);
  }
  // Backend now responds with an OperationAck envelope — surface it as-is.
  return (await resp.json()) as OperationAck;
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
  // Backend responds with UserResponse { user, version } — unwrap the profile.
  const data = (await resp.json()) as UserProfileMutationResponse;
  return data.user;
}

/** Activate a user (deactivates all others) */
export async function activateUser(
  userId: string,
  gatewayUrl = getGatewayUrl(),
): Promise<ActivateUserResponse> {
  const resp = await fetch(`${gatewayUrl}/api/users/${userId}/activate`, {
    method: "POST",
  });
  if (!resp.ok) {
    const err = await resp.json().catch(() => ({ error: resp.statusText }));
    throw new Error((err as { error?: string }).error ?? `Failed to activate user: ${resp.status}`);
  }
  // Backend responds with ActivateResponse { active_user_id, version }.
  return resp.json();
}

/** Reset onboarding wizard state. */
export async function resetOnboarding(): Promise<{ status: string; source: string }> {
  localStorage.removeItem("acowork_onboarding");
  return { status: "frontend_only", source: "local" };
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
 * Fetch the LSP Relay endpoint of the node hosting an agent.
 *
 * ADR-055 §6.7 (Phase 4): the relay is a node-local sidecar, so the
 * endpoint must be resolved per agent via
 * `GET /api/agents/{id}/lsp-endpoint`. Desktop App and Agent Runtime
 * use this to discover the relay, then connect directly.
 *
 * Returns `{ ready: false, endpoint: null }` when the hosting node's
 * relay has not published a ready state.
 */
export async function fetchAgentLspEndpoint(
  agentId: string,
  gatewayUrl = getGatewayUrl(),
): Promise<AgentLspEndpointResponse> {
  const resp = await fetch(
    `${gatewayUrl}/api/agents/${encodeURIComponent(agentId)}/lsp-endpoint`,
  );
  if (!resp.ok) throw new Error(`Failed to fetch LSP endpoint: ${resp.status}`);
  return resp.json();
}

/**
 * Build the base HTTP URL for the LSP Relay serving an agent.
 *
 * Returns `null` if the relay is not available (or no agent id was
 * provided).
 */
export async function getLspRelayUrl(
  agentId?: string,
  gatewayUrl = getGatewayUrl(),
): Promise<string | null> {
  if (!agentId) return null;
  return getCachedLspRelayEndpoint(agentId, gatewayUrl);
}

// ── LSP Relay direct API (servers / status / install) ───────────────────
//
// These functions call the LSP Relay directly (not through the Gateway).
// The caller must provide the relay base URL, typically obtained via
// `getLspRelayUrl()`.

/**
 * Fetch the configured LSP server list without probing `PATH`.
 *
 * The relay handler reads from a process-lifetime `OnceLock`, so this
 * endpoint is intentionally fast (no fork-exec, no PATH lookup). The
 * harness UI uses it on initial load to render the language list
 * **immediately**, then kicks off a separate `fetchLspStatus` /
 * `fetchLspServersWithStatus` call to resolve per-language install
 * badges incrementally.
 *
 * The returned `LspServersConfig` is the same shape as the `servers`
 * field of `LspServersWithStatus`.
 */
export async function fetchLspServers(
  relayUrl: string,
): Promise<LspServersConfig> {
  const resp = await fetch(`${relayUrl}/api/lsp/servers`);
  if (!resp.ok) {
    throw new Error(`Failed to fetch LSP servers: ${resp.status}`);
  }
  return resp.json();
}

/**
 * Fetch configured LSP servers together with per-language install status
 * in a single round-trip.
 *
 * Returns both the configured server list AND the per-language install
 * status in one call. The backend runs the PATH probes with bounded
 * concurrency (4 in flight), keeping total wall time roughly bounded
 * by a single probe timeout (~2s worst case).
 *
 * Results are cached on the LSP Relay side keyed by canonical language
 * with a configurable TTL (default 30 minutes). Pass `force = true` to
 * bypass the cache and re-probe every language — use this only on the
 * top-bar Refresh button, where a full re-probe is the user-intent.
 *
 * **Not the preferred initial-load call** — the harness UI uses the
 * two-step `fetchLspServers` + `fetchLspStatus` pair instead so the
 * list renders immediately and badges resolve incrementally. This
 * endpoint remains available for callers that want the combined payload
 * (e.g. CLI tools, one-shot scripts).
 */
export async function fetchLspServersWithStatus(
  relayUrl: string,
  options: { force?: boolean } = {},
): Promise<LspServersWithStatus> {
  const qs = options.force ? "?force=true" : "";
  const resp = await fetch(`${relayUrl}/api/lsp/servers-with-status${qs}`);
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
 * Results are cached on the LSP Relay side keyed by canonical language
 * with a configurable TTL (default 30 minutes). The harness UI does
 * **not** pass `force = true` here so the first call within TTL is
 * essentially free (HashMap read on the server); the top-bar Refresh
 * button uses `force = true` for a full re-probe.
 *
 * **`force = true` is rarely useful at this endpoint** — prefer
 * {@link fetchLspStatusForLanguage} for per-language forced probes,
 * since the batch endpoint still probes every language when forcing.
 */
export async function fetchLspStatus(
  relayUrl: string,
  options: { force?: boolean } = {},
): Promise<LspServerStatusEntry[]> {
  const qs = options.force ? "?force=true" : "";
  const resp = await fetch(`${relayUrl}/api/lsp/status${qs}`);
  if (!resp.ok) throw new Error(`Failed to fetch LSP status: ${resp.status}`);
  return resp.json();
}

/**
 * Re-probe LSP installation status for a single language.
 *
 * Preferred over {@link fetchLspStatus} for the harness UI's per-row
 * "Check Status" button — the previous behavior fetched the entire
 * status array (probing every language in the config) just to update
 * one row's badge. This endpoint probes only the requested language
 * (or hits the cache) and returns a single `LspServerStatusEntry`
 * object, not an array.
 *
 * The relay canonicalizes the input first, so language aliases (`js` →
 * `typescript`, `yml` → `yaml`, etc.) resolve cleanly. Unknown
 * languages return 404 — wrap in try/catch if the caller doesn't
 * filter by configured languages first.
 *
 * Pass `force = true` to bypass the cache for the single language
 * (e.g. immediately after an install). The relay's install endpoint
 * also drops the cache entry on success, so `force = true` is rarely
 * needed for the install flow.
 */
export async function fetchLspStatusForLanguage(
  relayUrl: string,
  language: string,
  options: { force?: boolean } = {},
): Promise<LspServerStatusEntry> {
  const qs = options.force ? "?force=true" : "";
  const encoded = encodeURIComponent(language);
  const resp = await fetch(`${relayUrl}/api/lsp/status/${encoded}${qs}`);
  if (resp.status === 404) {
    throw new Error(`Unknown LSP language: ${language}`);
  }
  if (!resp.ok) {
    throw new Error(
      `Failed to fetch LSP status for ${language}: ${resp.status}`,
    );
  }
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


// ── Settings API ────────────────────────────────────────────────────────
//
// ADR-056: Global default compact model. The Gateway persists this on the
// `provider_list.json` top-level `default_compact_model` field and pushes
// it as part of `acowork/global/providers` (AvailableProviders) via MQTT
// retained, so Runtimes can apply the three-tier distillation fallback.

/**
 * `GET /api/settings/default-compact-model` — read the user's current
 * global pick. Returns `null` when not configured.
 */
export async function getDefaultCompactModel(
  gatewayUrl = getGatewayUrl(),
): Promise<CompactModelRef | null> {
  const resp = await fetch(`${gatewayUrl}/api/settings/default-compact-model`);
  if (!resp.ok) {
    throw new Error(`Failed to fetch default compact model: ${resp.status}`);
  }
  const data = (await resp.json()) as DefaultCompactModelResponse;
  return data.default_compact_model ?? null;
}

/**
 * `PUT /api/settings/default-compact-model` — set or clear the global
 * default. Pass `null` to clear (Runtime then falls back to provider
 * compact_model and current chat model only).
 *
 * Returns the new value (as persisted by the Gateway).
 *
 * Throws on validation failure (unknown provider_id, or model_id not
 * belonging to that provider) — Gateway returns HTTP 422 with the
 * `error` field set (ADR-056 §4.1).
 */
export async function setDefaultCompactModel(
  ref: CompactModelRef | null,
  gatewayUrl = getGatewayUrl(),
): Promise<CompactModelRef | null> {
  const resp = await fetch(`${gatewayUrl}/api/settings/default-compact-model`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ default_compact_model: ref }),
  });
  const data = (await resp.json().catch(() => ({}))) as {
    default_compact_model?: CompactModelRef | null;
    error?: string;
  };
  if (!resp.ok) {
    throw new Error(data.error ?? `Failed to set default compact model: ${resp.status}`);
  }
  return data.default_compact_model ?? null;
}

/**
 * `GET /api/nodes` — list all known Node Agents (online + offline),
 * sorted by node_id (ADR-055 §6.13.3 / Phase 3g).
 *
 * Returns an empty list when the Gateway has no node registry (MQTT
 * disabled) or when no node has ever reported.
 */
export async function fetchNodes(gatewayUrl = getGatewayUrl()): Promise<NodeInfo[]> {
  const resp = await fetch(`${gatewayUrl}/api/nodes`);
  if (!resp.ok) {
    throw new Error(`Failed to fetch nodes: ${resp.status}`);
  }
  return (await resp.json()) as NodeInfo[];
}

// ── Structured error codes (ADR-059 §6.3) ──────────────────────────────
//
// The Gateway's mutation APIs fail with `{ error, code, structured? }`
// where `structured.code` is one of the 5 closed protocol codes below.
// Clients MUST branch on the machine-readable code — never on
// human-readable text (the old "503 / never enrolled" matching is
// gone, Phase 5.5).

export const STRUCTURED_ERROR_CODES = {
  dependencyNotReady: "dependency_not_ready",
  operationUncertain: "operation_uncertain",
  operationExpired: "operation_expired",
  resourceVersionConflict: "resource_version_conflict",
  handshakeTimeout: "handshake_timeout",
} as const;

/** Parsed structured Gateway error (ADR-059 §6.3). */
export interface StructuredGatewayError {
  status: number;
  /** Human-readable message (diagnostics only — never branch on it). */
  error: string;
  /** Machine-readable protocol code, when the body carried one. */
  code?: string;
  /** Aggregated bootstrap phase at failure time (SCREAMING_SNAKE_CASE). */
  current_phase?: string;
  /** Subsystem-level diagnostic (e.g. "node.local not ready"). */
  phase_detail?: string;
  /** Operation id of the failed mutation (operation_uncertain). */
  operation_id?: string;
  /** Current resource version (resource_version_conflict). */
  current_version?: number;
  /** The version the client expected (resource_version_conflict). */
  client_expected_version?: number;
}

/**
 * Parse a Gateway error response into a [`StructuredGatewayError`].
 *
 * Returns `null` when the body is not a Gateway API error (e.g. an
 * HTML error page from a proxy) — callers then fall back to a plain
 * status-based message.
 */
export async function parseStructuredError(
  resp: Response,
): Promise<StructuredGatewayError | null> {
  const data = (await resp.json().catch(() => ({}))) as Record<string, unknown>;
  if (typeof data.error !== "string") return null;
  const structured = (data.structured ?? {}) as Record<string, unknown>;
  const code = typeof structured.code === "string" ? structured.code : undefined;
  return {
    status: resp.status,
    error: data.error,
    code,
    current_phase: typeof structured.current_phase === "string" ? structured.current_phase : undefined,
    phase_detail: typeof structured.phase_detail === "string" ? structured.phase_detail : undefined,
    operation_id: typeof structured.operation_id === "string" ? structured.operation_id : undefined,
    current_version: typeof structured.current_version === "number" ? structured.current_version : undefined,
    client_expected_version: typeof structured.client_expected_version === "number" ? structured.client_expected_version : undefined,
  };
}

/** True when the error carries the `dependency_not_ready` code. */
export function isDependencyNotReady(err: StructuredGatewayError | null): boolean {
  return err?.code === STRUCTURED_ERROR_CODES.dependencyNotReady;
}

/** True when the error carries the `operation_uncertain` code. */
export function isOperationUncertain(err: StructuredGatewayError | null): boolean {
  return err?.code === STRUCTURED_ERROR_CODES.operationUncertain;
}

/** True when the error carries the `operation_expired` code. */
export function isOperationExpired(err: StructuredGatewayError | null): boolean {
  return err?.code === STRUCTURED_ERROR_CODES.operationExpired;
}

/** True when the error carries the `resource_version_conflict` code. */
export function isResourceVersionConflict(err: StructuredGatewayError | null): boolean {
  return err?.code === STRUCTURED_ERROR_CODES.resourceVersionConflict;
}

/** True when the error carries the `handshake_timeout` code. */
export function isHandshakeTimeout(err: StructuredGatewayError | null): boolean {
  return err?.code === STRUCTURED_ERROR_CODES.handshakeTimeout;
}

/**
 * Client handling strategy per code (ADR-059 §6.3):
 *
 * - `dependency_not_ready` — retry once the bootstrap phase is READY:
 *   poll `get_bootstrap` (Tauri) or `GET /api/bootstrap`, then resubmit.
 * - `operation_uncertain` — the mutation may or may not have applied
 *   (MQTT dropped / Gateway restarted). NEVER blindly retry: surface
 *   the `operation_id` and let the user re-check later.
 * - `operation_expired` — the operation's lease expired; re-submit as
 *   a fresh operation.
 * - `resource_version_conflict` — the client's `expected_version` is
 *   stale (typically a Gateway restart). Re-fetch the bootstrap
 *   snapshot for the new `version` and let the user confirm re-submit.
 * - `handshake_timeout` — a readiness probe timed out; retry the
 *   handshake.
 */


