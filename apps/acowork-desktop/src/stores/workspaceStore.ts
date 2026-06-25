import { create } from "zustand";
import { useSettingsStore } from "./settingsStore";
import { useChatStore } from "./chatStore";
import { DEFAULT_GATEWAY_URL } from "../lib/config";

/** Single workspace directory entry — matches Gateway API response */
interface WorkspaceDir {
  id: string;
  path: string;
  alias: string | null;
  access: "read-only" | "read-write";
  added_at: string;
  /** Deprecated: replaced by session-level workspace selection (sessionWorkspaceMap). */
  is_current?: boolean;
  /** Legacy field for backward compat; frontend reads sessionWorkspaceMap instead. */
  last_active?: boolean;
  select_count: number;
  last_selected_at: string | null;
  /** Prompt file to inject into system prompt (e.g. "CLAUDE.md", "AGENTS.md"). */
  prompt_file: string | null;
}

/** Single file/directory entry from the tree API — matches Gateway TreeResponse.entries */
export interface TreeEntry {
  name: string;
  /** "file" or "directory" */
  type: string;
  size?: number;
  modified?: string;
  childrenCount?: number;
}

/** Tree API response — matches Gateway TreeResponse */
export interface TreeResponse {
  root: string;
  path: string;
  entries: TreeEntry[];
}

/** Single filename-search result — matches Gateway FindResponse.matches */
export interface FileFindMatch {
  name: string;
  /** Forward-slash relative path within the workspace */
  relPath: string;
  /** "file" | "directory" — currently the endpoint only returns "file" */
  type: string;
  /** Heuristic score (higher = better match). */
  score: number;
}

/** Filename-search API response — matches Gateway FindResponse */
export interface FileFindResponse {
  root: string;
  scanned: number;
  truncated: boolean;
  matches: FileFindMatch[];
}

/** Cache key: `${agentId}:${workspaceId}:${relPath}` → TreeEntry[] */
type TreeCacheKey = string;

interface WorkspaceState {
  workspaces: WorkspaceDir[];
  /** Per-session current workspace selection. "__agent_home__" = agent home. */
  sessionWorkspaceMap: Record<string, string>;
  loading: boolean;
  /** Tree cache: agentId:workspaceId:relativePath → TreeEntry[] */
  treeCache: Record<TreeCacheKey, TreeEntry[]>;
  /** Workspace root path per agent+workspace (from tree API response) */
  treeRoots: Record<string, string>;
  /** Paths currently being fetched (to avoid duplicate requests) */
  treeLoadingPaths: Set<string>;

  // Fetch workspace list for a given agent
  fetchWorkspaces: (agentId: string) => Promise<void>;

  // Set current workspace for a specific session (preferred API)
  setSessionWorkspace: (agentId: string, sessionId: string, workspaceId: string) => Promise<void>;

  // Legacy: set current workspace using the active session (backward compat)
  setCurrentWorkspace: (agentId: string, workspaceId: string) => Promise<void>;

  // Synchronous local-only setter — used by chatStore/sessionStore to keep
  // sessionWorkspaceMap consistent without an API roundtrip.
  setSessionWorkspaceLocal: (sessionId: string, workspaceId: string) => void;

  // Bulk-sync session workspaces from fetchSessions / activate_session.
  // Accepts the raw session list; removes stale entries automatically.
  syncSessionWorkspaces: (sessions: Array<{ session_id: string; workspace_id?: string | null }>) => void;

  // Get current workspace ID for a session (defaults to "__agent_home__")
  getSessionWorkspaceId: (sessionId: string) => string;

  // Fetch directory tree for a given agent + workspace + relative path
  fetchTree: (agentId: string, workspaceId: string, relPath?: string) => Promise<TreeEntry[] | null>;

  // Server-side filename search. The Gateway walks the workspace
  // (gitignore-aware) and returns ranked matches in one request.
  findFiles: (
    agentId: string,
    workspaceId: string,
    query: string,
    limit?: number,
    signal?: AbortSignal,
  ) => Promise<FileFindResponse | null>;

  // Get cached tree entries
  getCachedTree: (agentId: string, workspaceId: string, relPath: string) => TreeEntry[] | undefined;

  // Invalidate tree cache for an agent (e.g. when workspace changes)
  invalidateTreeCache: (agentId: string) => void;

  // Create a new empty file in the workspace
  createFile: (agentId: string, workspaceId: string, path: string) => Promise<boolean>;

  // Create a new directory in the workspace
  createDir: (agentId: string, workspaceId: string, path: string) => Promise<boolean>;

  // Delete a file from the workspace
  deleteFile: (agentId: string, workspaceId: string, path: string) => Promise<boolean>;

  // Delete a directory from the workspace (recursive)
  deleteDir: (agentId: string, workspaceId: string, path: string) => Promise<boolean>;

  // Copy a file or directory within the workspace
  copyItem: (agentId: string, workspaceId: string, source: string, dest: string) => Promise<boolean>;

  // Clipboard for copy/paste — stores the source entry to be pasted
  copiedEntry: { agentId: string; workspaceId: string; path: string; type: "file" | "directory" } | null;
  setCopiedEntry: (entry: { agentId: string; workspaceId: string; path: string; type: "file" | "directory" } | null) => void;

  // Set/unset prompt file for workspace (e.g. CLAUDE.md, AGENTS.md)
  setPromptFile: (agentId: string, workspaceId: string, promptFile: string | null) => Promise<boolean>;

  // Clear state on agent switch
  reset: () => void;
}

/** Helper: resolve Gateway URL from settings store, fallback to default */
function getGatewayUrl(): string {
  return useSettingsStore.getState().gatewayUrl || DEFAULT_GATEWAY_URL;
}

/** Monotonic counter to discard stale async responses (race-condition guard) */
let requestSeq = 0;

export const useWorkspaceStore = create<WorkspaceState>((set, get) => ({
  workspaces: [],
  sessionWorkspaceMap: {},
  loading: false,
  treeCache: {},
  treeRoots: {},
  treeLoadingPaths: new Set<string>(),

  fetchWorkspaces: async (agentId: string) => {
    const seq = ++requestSeq;
    set({ loading: true });
    try {
      const baseUrl = getGatewayUrl();
      const resp = await fetch(`${baseUrl}/api/agents/${agentId}/workspaces`);
      if (!resp.ok) {
        console.error("[WorkspaceStore] fetchWorkspaces failed:", resp.status, resp.statusText);
        set({ loading: false });
        return;
      }
      const data = (await resp.json()) as { workspaces: WorkspaceDir[] };
      const workspaces = data.workspaces || [];
      // Discard stale response if a newer request has been issued
      if (seq !== requestSeq) return;
      set({
        workspaces,
        loading: false,
      });
    } catch (e) {
      console.error("[WorkspaceStore] fetchWorkspaces error:", e);
      if (seq !== requestSeq) return;
      set({ loading: false });
    }
  },

  setSessionWorkspace: async (agentId: string, sessionId: string, workspaceId: string) => {
    const seq = ++requestSeq;
    const prevWorkspaces = get().workspaces;
    const prevMap = { ...get().sessionWorkspaceMap };
    try {
      const baseUrl = getGatewayUrl();
      const resp = await fetch(
        `${baseUrl}/api/agents/${agentId}/workspaces/current?session_id=${encodeURIComponent(sessionId)}`,
        {
          method: "PUT",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ workspace_id: workspaceId }),
        },
      );
      if (!resp.ok) {
        console.error("[WorkspaceStore] setSessionWorkspace failed:", resp.status, resp.statusText);
        return;
      }
      // API returns the updated workspace list after switching
      const data = (await resp.json()) as { workspaces: WorkspaceDir[] };
      const workspaces = data.workspaces || [];
      // Discard stale response if a newer request has been issued
      if (seq !== requestSeq) return;
      set({
        workspaces,
        sessionWorkspaceMap: {
          ...get().sessionWorkspaceMap,
          [sessionId]: workspaceId,
        },
      });
    } catch (e) {
      console.error("[WorkspaceStore] setSessionWorkspace error:", e);
      // Revert to previous state on failure (only if still the latest request)
      if (seq !== requestSeq) return;
      set({ workspaces: prevWorkspaces, sessionWorkspaceMap: prevMap });
    }
  },

  setCurrentWorkspace: async (agentId: string, workspaceId: string) => {
    // Legacy wrapper: resolve active session ID and delegate to setSessionWorkspace
    const activeSessionId = useChatStore.getState().getActiveSessionId(agentId);
    if (!activeSessionId) {
      console.warn("[WorkspaceStore] setCurrentWorkspace: no active session for agent", agentId);
      return;
    }
    return get().setSessionWorkspace(agentId, activeSessionId, workspaceId);
  },

  setSessionWorkspaceLocal: (sessionId: string, workspaceId: string) => {
    set((state) => ({
      sessionWorkspaceMap: { ...state.sessionWorkspaceMap, [sessionId]: workspaceId },
    }));
  },

  syncSessionWorkspaces: (sessions) => {
    set((state) => {
      const next = { ...state.sessionWorkspaceMap };
      let changed = false;
      for (const s of sessions) {
        const wsId = s.workspace_id;
        if (wsId && wsId !== "__agent_home__") {
          if (next[s.session_id] !== wsId) {
            next[s.session_id] = wsId;
            changed = true;
          }
        } else if (s.session_id in next) {
          delete next[s.session_id];
          changed = true;
        }
      }
      return changed ? { sessionWorkspaceMap: next } : {};
    });
  },

  getSessionWorkspaceId: (sessionId: string) => {
    return get().sessionWorkspaceMap[sessionId] ?? "__agent_home__";
  },

  fetchTree: async (agentId: string, workspaceId: string, relPath?: string) => {
    const path = relPath ?? "";
    const cacheKey = `${agentId}:${workspaceId}:${path}`;

    // Deduplicate in-flight requests
    if (get().treeLoadingPaths.has(cacheKey)) return null;

    set((state) => ({
      treeLoadingPaths: new Set(state.treeLoadingPaths).add(cacheKey),
    }));

    try {
      const baseUrl = getGatewayUrl();
      const params = new URLSearchParams();
      if (workspaceId && workspaceId !== "__agent_home__") {
        params.set("workspace_id", workspaceId);
      }
      if (path) {
        params.set("path", path);
      }
      const qs = params.toString();
      const url = `${baseUrl}/api/agents/${agentId}/workspaces/tree${qs ? `?${qs}` : ""}`;
      const resp = await fetch(url);
      if (!resp.ok) {
        console.error("[WorkspaceStore] fetchTree failed:", resp.status, resp.statusText);
        return null;
      }
      const data = (await resp.json()) as TreeResponse;
      const rootKey = `${agentId}:${workspaceId}`;
      set((state) => ({
        treeCache: { ...state.treeCache, [cacheKey]: data.entries },
        treeRoots: { ...state.treeRoots, [rootKey]: data.root },
        treeLoadingPaths: (() => {
          const next = new Set(state.treeLoadingPaths);
          next.delete(cacheKey);
          return next;
        })(),
      }));
      return data.entries;
    } catch (e) {
      console.error("[WorkspaceStore] fetchTree error:", e);
      set((state) => {
        const next = new Set(state.treeLoadingPaths);
        next.delete(cacheKey);
        return { treeLoadingPaths: next };
      });
      return null;
    }
  },

  getCachedTree: (agentId: string, workspaceId: string, relPath: string) => {
    return get().treeCache[`${agentId}:${workspaceId}:${relPath}`];
  },

  findFiles: async (
    agentId: string,
    workspaceId: string,
    query: string,
    limit?: number,
    signal?: AbortSignal,
  ) => {
    const trimmed = query.trim();
    if (!trimmed) return null;
    try {
      const baseUrl = getGatewayUrl();
      const params = new URLSearchParams({ q: trimmed });
      if (workspaceId && workspaceId !== "__agent_home__") {
        params.set("workspace_id", workspaceId);
      }
      if (limit && limit > 0) {
        params.set("limit", String(limit));
      }
      const resp = await fetch(
        `${baseUrl}/api/agents/${agentId}/workspaces/find?${params.toString()}`,
        { signal },
      );
      if (!resp.ok) {
        // 404 = agent not running. Treat as no results so the UI stays
        // quiet instead of logging a misleading error.
        if (resp.status === 404) return null;
        console.error("[WorkspaceStore] findFiles failed:", resp.status, resp.statusText);
        return null;
      }
      return (await resp.json()) as FileFindResponse;
    } catch (e) {
      // AbortError is expected when the caller cancels an in-flight
      // request (e.g. user kept typing); don't log it as an error.
      if (e instanceof DOMException && e.name === "AbortError") return null;
      console.error("[WorkspaceStore] findFiles error:", e);
      return null;
    }
  },

  invalidateTreeCache: (agentId: string) => {
    set((state) => {
      const nextCache: Record<string, TreeEntry[]> = {};
      for (const [key, val] of Object.entries(state.treeCache)) {
        if (!key.startsWith(`${agentId}:`)) {
          nextCache[key] = val;
        }
      }
      const nextRoots: Record<string, string> = {};
      for (const [key, val] of Object.entries(state.treeRoots)) {
        if (!key.startsWith(`${agentId}:`)) {
          nextRoots[key] = val;
        }
      }
      return { treeCache: nextCache, treeRoots: nextRoots };
    });
  },

  createFile: async (agentId: string, workspaceId: string, path: string) => {
    try {
      const baseUrl = getGatewayUrl();
      const params = new URLSearchParams();
      if (workspaceId && workspaceId !== "__agent_home__") {
        params.set("workspace_id", workspaceId);
      }
      const qs = params.toString();
      const resp = await fetch(`${baseUrl}/api/agents/${agentId}/workspaces/file${qs ? `?${qs}` : ""}`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ path }),
      });
      if (!resp.ok) {
        const body = await resp.text().catch(() => "<unreadable>");
        console.error("[WorkspaceStore] createFile failed:", resp.status, resp.statusText, body);
        return false;
      }
      return true;
    } catch (e) {
      console.error("[WorkspaceStore] createFile error:", e);
      return false;
    }
  },

  createDir: async (agentId: string, workspaceId: string, path: string) => {
    try {
      const baseUrl = getGatewayUrl();
      const params = new URLSearchParams();
      if (workspaceId && workspaceId !== "__agent_home__") {
        params.set("workspace_id", workspaceId);
      }
      const qs = params.toString();
      const resp = await fetch(`${baseUrl}/api/agents/${agentId}/workspaces/dir${qs ? `?${qs}` : ""}`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ path }),
      });
      if (!resp.ok) {
        const body = await resp.text().catch(() => "<unreadable>");
        console.error("[WorkspaceStore] createDir failed:", resp.status, resp.statusText, body);
        return false;
      }
      return true;
    } catch (e) {
      console.error("[WorkspaceStore] createDir error:", e);
      return false;
    }
  },

  deleteFile: async (agentId: string, workspaceId: string, path: string) => {
    try {
      const baseUrl = getGatewayUrl();
      const params = new URLSearchParams();
      if (workspaceId && workspaceId !== "__agent_home__") {
        params.set("workspace_id", workspaceId);
      }
      const qs = params.toString();
      const resp = await fetch(`${baseUrl}/api/agents/${agentId}/workspaces/file${qs ? `?${qs}` : ""}`, {
        method: "DELETE",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ path }),
      });
      if (!resp.ok) {
        const body = await resp.text().catch(() => "<unreadable>");
        console.error("[WorkspaceStore] deleteFile failed:", resp.status, resp.statusText, body);
        return false;
      }
      return true;
    } catch (e) {
      console.error("[WorkspaceStore] deleteFile error:", e);
      return false;
    }
  },

  deleteDir: async (agentId: string, workspaceId: string, path: string) => {
    try {
      const baseUrl = getGatewayUrl();
      const params = new URLSearchParams();
      if (workspaceId && workspaceId !== "__agent_home__") {
        params.set("workspace_id", workspaceId);
      }
      const qs = params.toString();
      const resp = await fetch(`${baseUrl}/api/agents/${agentId}/workspaces/dir${qs ? `?${qs}` : ""}`, {
        method: "DELETE",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ path }),
      });
      if (!resp.ok) {
        const body = await resp.text().catch(() => "<unreadable>");
        console.error("[WorkspaceStore] deleteDir failed:", resp.status, resp.statusText, body);
        return false;
      }
      return true;
    } catch (e) {
      console.error("[WorkspaceStore] deleteDir error:", e);
      return false;
    }
  },

  copyItem: async (agentId: string, workspaceId: string, source: string, dest: string) => {
    try {
      const baseUrl = getGatewayUrl();
      const params = new URLSearchParams();
      if (workspaceId && workspaceId !== "__agent_home__") {
        params.set("workspace_id", workspaceId);
      }
      const qs = params.toString();
      const resp = await fetch(`${baseUrl}/api/agents/${agentId}/workspaces/copy${qs ? `?${qs}` : ""}`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ source, dest }),
      });
      if (!resp.ok) {
        const body = await resp.text().catch(() => "<unreadable>");
        console.error("[WorkspaceStore] copyItem failed:", resp.status, resp.statusText, body);
        return false;
      }
      return true;
    } catch (e) {
      console.error("[WorkspaceStore] copyItem error:", e);
      return false;
    }
  },

  copiedEntry: null,

  setCopiedEntry: (entry) => {
    set({ copiedEntry: entry });
  },

  setPromptFile: async (agentId: string, workspaceId: string, promptFile: string | null) => {
    try {
      const baseUrl = getGatewayUrl();
      const resp = await fetch(`${baseUrl}/api/agents/${agentId}/workspaces/${workspaceId}/prompt-file`, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ prompt_file: promptFile }),
      });
      if (!resp.ok) {
        const body = await resp.text().catch(() => "<unreadable>");
        console.error("[WorkspaceStore] setPromptFile failed:", resp.status, resp.statusText, body);
        return false;
      }
      // Update the local workspace state
      const updated = await resp.json() as WorkspaceDir;
      set((state) => ({
        workspaces: state.workspaces.map((ws) => (ws.id === workspaceId ? updated : ws)),
      }));
      return true;
    } catch (e) {
      console.error("[WorkspaceStore] setPromptFile error:", e);
      return false;
    }
  },

  reset: () => {
    set({ workspaces: [], sessionWorkspaceMap: {}, loading: false, treeCache: {}, treeRoots: {}, treeLoadingPaths: new Set(), copiedEntry: null });
  },
}));

export type { WorkspaceDir, WorkspaceState };
