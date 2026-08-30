/**
 * fileTree — HTTP client
 *
 * Single-responsibility wrapper around `fetch` for the workspace tree
 * endpoint. Lives apart from the cache so:
 *   - tests can swap the cache without dragging `fetch` along
 *   - we can later add auth headers / retry policy in one place
 *   - the URL building is testable independently (no AbortSignal noise)
 *
 * The AbortSignal is threaded through `fetch` so `treeCache.invalidate`
 * can cancel in-flight requests the moment a workspace is switched.
 */

import type { TreeResponse } from "./types";
import { with503Retry } from "../../lib/httpRetry";
import { log } from "../../lib/logger";

/**
 * Injected by the consumer. In production this is `globalThis.fetch`;
 * in tests it is a vi.fn() that records calls and resolves/rejects
 * on demand. Kept as an explicit dependency rather than an import so
 * the cache layer never touches the DOM globals.
 */
export type FetchLike = (input: string, init?: RequestInit) => Promise<Response>;

/** Returns the absolute Gateway URL (e.g. http://127.0.0.1:7333). */
export type GatewayUrlResolver = () => string;

export interface TreeClientOptions {
  fetch?: FetchLike;
  gatewayUrl: GatewayUrlResolver;
}

/**
 * Build a TreeFetcher for a given (agent, workspace, path). The closure
 * captures `opts` once and produces a function compatible with
 * `TreeCache.fetch(key, fetcher)`.
 *
 * The injected fetch (or `globalThis.fetch`) is read on every call,
 * NOT bound once at construction time. This matters for tests that use
 * `vi.stubGlobal("fetch", ...)` after the client was created — a
 * one-time bind would permanently lose the stub.
 *
 * The fetcher is wrapped in `with503Retry` so transient 503 responses
 * (Runtime port not yet registered in Gateway's reverse proxy) recover
 * transparently. The cache layer stays transport-agnostic — this is
 * the right boundary because:
 *   - SWR / dedup / abort semantics live in treeCache (no retry)
 *   - Retry-After / SHUTTING_DOWN / budget exhaustion live in
 *     httpRetry.with503Retry (no cache concerns)
 */
export function createTreeClient(opts: TreeClientOptions): (a: string, w: string, p: string) => (signal: AbortSignal) => Promise<TreeResponse> {
  return (agentId, workspaceId, relPath) => async (signal) => {
    const params = new URLSearchParams();
    if (workspaceId && workspaceId !== "__agent_home__") {
      params.set("workspace_id", workspaceId);
    }
    if (relPath) params.set("path", relPath);
    const qs = params.toString();
    const url = `${opts.gatewayUrl()}/api/agents/${agentId}/workspaces/tree${qs ? `?${qs}` : ""}`;

    const f = opts.fetch ?? globalThis.fetch;
    const resp = await with503Retry(
      (sig) => f(url, { signal: sig ?? signal }),
      { tag: `TreeClient.fetchTree(${agentId})`, logger: log, signal },
    );
    if (!resp.ok) {
      // Throw an Error carrying `status` so treeCache.classifyError
      // can map it into our `TreeError` union. Even after all retries
      // a 503 is surfaced as `http:503` so the UI can distinguish it
      // from "no workspaces configured" (an empty entries array).
      const err = new Error(`fetchTree ${resp.status} ${resp.statusText}`) as Error & {
        status: number;
        statusText: string;
      };
      err.status = resp.status;
      err.statusText = resp.statusText;
      throw err;
    }
    return (await resp.json()) as TreeResponse;
  };
}
