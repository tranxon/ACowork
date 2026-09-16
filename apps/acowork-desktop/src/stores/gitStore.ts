/**
 * gitStore — workspace Git Status Bar data layer (ADR-078).
 *
 * Mirrors the Runtime's `/git/*` DTOs (usecases/git_query.rs) and talks to
 * the Gateway reverse-proxy `/api/agents/{id}/git/*` (the Gateway never
 * touches `.git` — ADR-009 / run_gateway_fs_redline).
 *
 * Responsibilities:
 *   - fetchStatus / fetchDiff / fetchLog (with 503 retry + per-group
 *     inflight dedup for status)
 *   - expanded state of the GitStatusBar panel, per (agent, workspace)
 *     group — the watch-set reporter (workspaceFsWatch) reads this to add
 *     the workspace root to the demand-driven fs-watch set only while the
 *     panel is expanded (ADR-078 decision 8)
 *   - notifyFsChanged — debounced status refresh triggered by
 *     `acowork:workspace-fs-changed` events that hit the current group
 */

import { create } from "zustand";
import { getGatewayUrl } from "../lib/config";
import { with503Retry } from "../lib/httpRetry";
import { log } from "../lib/logger";

// ── DTOs (mirror of runtime usecases/git_query.rs) ─────────────────────────

export type GitIndexStatus =
  | "added"
  | "modified"
  | "deleted"
  | "renamed"
  | "conflicted"
  | "unmodified";
export type GitWorktreeStatus =
  | "modified"
  | "deleted"
  | "untracked"
  | "conflicted"
  | "unmodified";

export interface GitChangeDto {
  path: string;
  oldPath: string | null;
  index: GitIndexStatus;
  worktree: GitWorktreeStatus;
  staged: boolean;
}

export interface GitStatusResponse {
  isRepo: boolean;
  branch: string | null;
  error: string | null; // "not_a_repo" | "git_unavailable" | null
  truncated: boolean;
  changes: GitChangeDto[];
}

export interface GitDiffResponse {
  kind: "modified" | "untracked" | "deleted" | "binary" | "no_change";
  original: string;
  modified: string;
}

export interface GitCommitDto {
  hash: string;
  shortHash: string;
  author: string;
  date: string;
  subject: string;
}

export interface GitLogResponse {
  commits: GitCommitDto[];
}

/** Group key: `${agentId}\u0000${workspaceId}` (matches workspaceFsWatch). */
export type GitGroupKey = string;

/** Build the store's group key for an (agent, workspace) pair. */
export function gitGroupKey(agentId: string, workspaceId: string): GitGroupKey {
  return `${agentId}\u0000${workspaceId}`;
}

/** `__agent_home__` means "the default workspace" — sent as no workspace_id. */
const AGENT_HOME = "__agent_home__";

/** Debounce for fs-changed-triggered status refresh (ADR-078 decision 8). */
const FS_REFRESH_DEBOUNCE_MS = 300;

/** Composite cache key for `(groupKey, rev)`. Empty `rev` = working tree
 *  (the legacy default). Composed by joining with a vertical bar, which
 *  cannot appear in either input (groupKey is `${a}:${ws}`, rev is a
 *  git ref or empty string). Used everywhere the cache slot needs to
 *  distinguish "files in commit X" from "files in the working tree". */
export function gitViewKey(groupKey: GitGroupKey, rev: string): GitGroupKey {
  return rev ? `${groupKey}|${rev}` : groupKey;
}

interface GitStatusEntry {
  data: GitStatusResponse | null;
  loading: boolean;
  error: string | null;
  fetchedAt: number;
}

interface GitStore {
  expandedKey: GitGroupKey | null;
  /** Per-group view selection. The Git Status Bar history dropdown writes
   *  here; the panel reads via [`gitViewKey`] to look up the matching
   *  `status[viewKey]` slot. Empty string = working tree (the default). */
  viewingRev: Record<GitGroupKey, string>;
  /** Cache slots keyed by `gitViewKey(group, rev)` so switching the bar's
   *  history dropdown never trashes the working-tree cache, and vice
   *  versa. */
  status: Record<GitGroupKey, GitStatusEntry>;
  /** Inflight status promises, keyed by `gitViewKey`, for dedup. */
  _inflight: Record<GitGroupKey, Promise<GitStatusResponse | null>>;
  _fsTimer: Record<GitGroupKey, ReturnType<typeof setTimeout> | undefined>;

  isExpanded: (agentId: string, workspaceId: string) => boolean;
  setExpanded: (agentId: string, workspaceId: string, expanded: boolean) => void;
  /** Read the rev the group is currently viewing ("" = working tree). */
  getViewingRev: (agentId: string, workspaceId: string) => string;
  /** Switch the group to a different rev. `""` = working tree.
   *  Triggers a status fetch for the new view if no cache entry exists. */
  setViewingRev: (agentId: string, workspaceId: string, rev: string) => void;

  fetchStatus: (
    agentId: string,
    workspaceId: string,
    rev?: string,
  ) => Promise<GitStatusResponse | null>;
  fetchDiff: (
    agentId: string,
    workspaceId: string,
    path: string,
    baseRef?: string,
    headRef?: string,
  ) => Promise<GitDiffResponse>;
  fetchLog: (
    agentId: string,
    workspaceId: string,
    path?: string,
    limit?: number,
  ) => Promise<GitLogResponse>;

  /** Clear the cached status for a group (switch agent / workspace). */
  invalidate: (agentId: string, workspaceId: string) => void;
  /** Force a fresh status fetch (bypassing the cache) for the group's
   *  CURRENTLY-VIEWED rev. */
  refresh: (agentId: string, workspaceId: string) => Promise<void>;
  /** fs-changed event hit for the group → debounced refresh (decision 8). */
  notifyFsChanged: (agentId: string, workspaceId: string) => void;
}

export const useGitStore = create<GitStore>((set, get) => {
  async function httpGet<T>(
    path: string,
    params: Record<string, string>,
  ): Promise<T> {
    const base = getGatewayUrl();
    const qs = new URLSearchParams(params).toString();
    const url = `${base}${path}${qs ? `?${qs}` : ""}`;
    const resp = await with503Retry(
      (sig) => fetch(url, { signal: sig }),
      { tag: `gitStore.${path}`, logger: log },
    );
    if (!resp.ok) {
      let detail = "";
      try {
        detail = JSON.stringify(await resp.json());
      } catch {
        /* non-JSON error body */
      }
      throw new Error(
        `git ${path} ${resp.status} ${resp.statusText} ${detail}`.trim(),
      );
    }
    return (await resp.json()) as T;
  }

  function statusParams(workspaceId: string): Record<string, string> {
    return workspaceId && workspaceId !== AGENT_HOME
      ? { workspace_id: workspaceId }
      : {};
  }

  return {
    expandedKey: null,
    viewingRev: {},
    status: {},
    _inflight: {},
    _fsTimer: {},

    isExpanded: (agentId, workspaceId) =>
      get().expandedKey === gitGroupKey(agentId, workspaceId),

    getViewingRev: (agentId, workspaceId) =>
      get().viewingRev[gitGroupKey(agentId, workspaceId)] ?? "",

    setViewingRev: (agentId, workspaceId, rev) => {
      const key = gitGroupKey(agentId, workspaceId);
      if ((get().viewingRev[key] ?? "") === rev) return;
      set((s) => ({ viewingRev: { ...s.viewingRev, [key]: rev } }));
      // Trigger a fetch for the new view if no cache entry exists yet
      // (cache hits are handled by fetchStatus' own dedup + view-key
      // path). The panel reads `status[gitViewKey(group, rev)]` and
      // will fall back to the existing entry while the fetch is in
      // flight, so no flicker.
      void get().fetchStatus(agentId, workspaceId, rev);
    },

    setExpanded: (agentId, workspaceId, expanded) => {
      const key = gitGroupKey(agentId, workspaceId);
      const next = expanded ? key : null;
      if (get().expandedKey === next) return;
      set({ expandedKey: next });
      if (expanded) {
        // Expand → subscribe → pull initial status (the watch reporter
        // derives the root path from expandedKey; fetch converges the UI).
        void get().refresh(agentId, workspaceId);
      } else {
        // Collapse → cancel any pending debounced refresh for this group.
        const timer = get()._fsTimer[key];
        if (timer) {
          clearTimeout(timer);
          const timers = { ...get()._fsTimer };
          delete timers[key];
          set({ _fsTimer: timers });
        }
      }
    },

    fetchStatus: async (agentId, workspaceId, rev) => {
      const key = gitGroupKey(agentId, workspaceId);
      // The rev is also stored in `viewingRev` when callers don't pass
      // it explicitly — that's the legacy refresh() path. Read once so
      // both the cache key and the HTTP query agree.
      const effectiveRev =
        rev ?? get().viewingRev[key] ?? "";
      const viewKey = gitViewKey(key, effectiveRev);
      const inflight = get()._inflight[viewKey];
      if (inflight) return inflight;
      set((s) => ({
        status: {
          ...s.status,
          [viewKey]: {
            ...(s.status[viewKey] ?? { data: null, error: null, fetchedAt: 0 }),
            loading: true,
          },
        },
      }));
      let p: Promise<GitStatusResponse | null>;
      p = (async () => {
        try {
          const params: Record<string, string> = statusParams(workspaceId);
          if (effectiveRev) params.rev = effectiveRev;
          const data = await httpGet<GitStatusResponse>(
            `/api/agents/${agentId}/git/status`,
            params,
          );
          set((s) => ({
            status: {
              ...s.status,
              [viewKey]: {
                data,
                loading: false,
                error: null,
                fetchedAt: Date.now(),
              },
            },
          }));
          return data;
        } catch (e) {
          set((s) => ({
            status: {
              ...s.status,
              [viewKey]: {
                ...(s.status[viewKey] ?? { data: null, fetchedAt: 0 }),
                loading: false,
                error: e instanceof Error ? e.message : String(e),
              },
            },
          }));
          return null;
        } finally {
          // In-flight dedup means no second fetch can have taken this key
          // while we were running (a concurrent caller returns our promise
          // instead), so clearing the slot on completion is always safe.
          const inflight2 = get()._inflight;
          if (viewKey in inflight2) {
            const next = { ...inflight2 };
            delete next[viewKey];
            set({ _inflight: next });
          }
        }
      })();
      set((s) => ({ _inflight: { ...s._inflight, [viewKey]: p } }));
      return p;
    },

    fetchDiff: (agentId, workspaceId, path, baseRef = "HEAD", headRef = "") => {
      const q: Record<string, string> = {
        ...statusParams(workspaceId),
        path,
        base_ref: baseRef,
        head_ref: headRef,
      };
      return httpGet<GitDiffResponse>(`/api/agents/${agentId}/git/diff`, q);
    },

    fetchLog: (agentId, workspaceId, path, limit = 50) => {
      const params: Record<string, string> = {
        ...statusParams(workspaceId),
        limit: String(Math.min(limit, 200)),
      };
      if (path) params.path = path;
      return httpGet<GitLogResponse>(`/api/agents/${agentId}/git/log`, params);
    },

    invalidate: (agentId, workspaceId) => {
      const key = gitGroupKey(agentId, workspaceId);
      // Drop every view slot for this group (working tree + each commit
      // the user has peeked at via the history dropdown). The working
      // tree slot is keyed by the bare group key (`rev = ""`).
      const revs = new Set<string>([""]);
      const viewing = get().viewingRev[key];
      if (viewing) revs.add(viewing);
      set((s) => {
        const status = { ...s.status };
        for (const v of revs) delete status[gitViewKey(key, v)];
        return { status };
      });
    },

    refresh: async (agentId, workspaceId) => {
      // Force a fresh fetch of the group's CURRENTLY-VIEWED rev. fs-watcher
      // events target the working tree, so refreshing the view the panel
      // is rendering keeps the UI coherent without thrashing the
      // commit-view cache.
      const key = gitGroupKey(agentId, workspaceId);
      const rev = get().viewingRev[key] ?? "";
      await get().fetchStatus(agentId, workspaceId, rev);
    },

    notifyFsChanged: (agentId, workspaceId) => {
      const key = gitGroupKey(agentId, workspaceId);
      // Only refresh groups whose panel is expanded (ADR-078 decision 8:
      // expanded → subscribed → fs-changed triggers refresh).
      if (get().expandedKey !== key) return;
      const existing = get()._fsTimer[key];
      if (existing) clearTimeout(existing);
      const timer = setTimeout(() => {
        const timers = { ...get()._fsTimer };
        delete timers[key];
        set({ _fsTimer: timers });
        void get().refresh(agentId, workspaceId);
      }, FS_REFRESH_DEBOUNCE_MS);
      set((s) => ({ _fsTimer: { ...s._fsTimer, [key]: timer } }));
    },
  };
});
