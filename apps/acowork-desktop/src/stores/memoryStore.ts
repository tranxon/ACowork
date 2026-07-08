import { create } from "zustand";
import type {
  MemoryNodeResponse,
  MemoryNodesListResponse,
  MemoryStatsResponse,
  DeleteNodeResponse,
  ConsolidateResponse,
} from "../lib/types";
import { getGatewayUrl } from "../lib/config";
import {
  fetchEmbeddingModels,
  startMigration,
} from "../lib/gateway-api";
import { useGatewayStore } from "./gatewayStore";

interface MemoryFilters {
  type: "All" | "Knowledge" | "Episodic" | "Procedural" | "Autobiographical";
  keyword: string;
  timeRange: "1h" | "1d" | "7d" | "30d" | "all";
}

interface MemoryStore {
  nodes: MemoryNodeResponse[];
  total: number;
  stats: MemoryStatsResponse | null;
  selectedNodeId: number | null;

  filters: MemoryFilters;
  page: number;
  pageSize: number;

  loading: boolean;
  error: string | null;
  consolidateMessage: string | null;

  /**
   * Set while a "Rebuild Index" migration is in flight for the currently
   * selected agent. Driven by the same harness /api/embedding-models/{id}/
   * start-migration endpoint the Harness tab already uses — we just call it
   * from the memory panel when stored_dim ≠ model_dim.
   *
   * `rebuildPollingRef` (closure-scoped, not in state) holds the interval
   * handle so we can clear it on completion or on agent switch.
   */
  migrationInProgress: boolean;

  // Actions
  fetchNodes: (agentId: string) => Promise<void>;
  fetchStats: (agentId: string) => Promise<void>;
  deleteNode: (agentId: string, nodeId: number) => Promise<void>;
  consolidate: (agentId: string, force?: boolean) => Promise<void>;
  /**
   * Rebuild the Grafeo HNSW vector index for `agentId` using the currently
   * active embedding model. Re-embeds every node so that mismatched-dim stores
   * (or stores that pre-date the embedding provider) become searchable again.
   *
   * Internally calls `startMigration(activeModelId, [agentId])` on the same
   * endpoint the Harness tab uses, then polls `pollMigrationProgress()` every
   * 2s until the agent reports `done` or an error.
   */
  rebuildIndex: (agentId: string) => Promise<void>;
  setFilters: (partial: Partial<MemoryFilters>) => void;
  setPage: (page: number) => void;
  setSelectedNodeId: (id: number | null) => void;
  clearMemory: () => void;
}

// Polling-interval handle for the in-flight Rebuild Index action. Kept outside
// the store so clearing memory / switching agents cancels any active poll
// without needing to thread the handle through state.
let rebuildPollingTimer: ReturnType<typeof setInterval> | null = null;

export const useMemoryStore = create<MemoryStore>((set, get) => ({
  nodes: [],
  total: 0,
  stats: null,
  selectedNodeId: null,
  filters: { type: "All", keyword: "", timeRange: "all" },
  page: 1,
  pageSize: 20,
  loading: false,
  error: null,
  consolidateMessage: null,
  migrationInProgress: false,

  fetchNodes: async (agentId) => {
    const { page, pageSize, filters } = get();
    set({ loading: true, error: null });
    try {
      const params = new URLSearchParams({
        page: String(page),
        size: String(pageSize),
      });
      if (filters.type !== "All") params.set("type", filters.type);
      if (filters.keyword) params.set("keyword", filters.keyword);
      if (filters.timeRange !== "all") params.set("time_range", filters.timeRange);

      const res = await fetch(`${getGatewayUrl()}/api/agents/${agentId}/memory/nodes?${params}`);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const data: MemoryNodesListResponse = await res.json();
      set({ nodes: data.nodes, total: data.total, loading: false });
    } catch (e) {
      set({ loading: false, error: e instanceof Error ? e.message : "Unknown error" });
    }
  },

  fetchStats: async (agentId) => {
    try {
      const res = await fetch(`${getGatewayUrl()}/api/agents/${agentId}/memory/stats`);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const data: MemoryStatsResponse = await res.json();
      set({ stats: data });
    } catch (e) {
      console.error("Failed to fetch memory stats:", e);
    }
  },

  deleteNode: async (agentId, nodeId) => {
    try {
      const res = await fetch(`${getGatewayUrl()}/api/agents/${agentId}/memory/nodes/${nodeId}`, {
        method: "DELETE",
      });
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const data: DeleteNodeResponse = await res.json();
      if (data.deleted) {
        set((s) => ({
          nodes: s.nodes.filter((n) => n.node_id !== nodeId),
          total: s.total - 1,
          selectedNodeId: s.selectedNodeId === nodeId ? null : s.selectedNodeId,
        }));
        // Refresh stats
        get().fetchStats(agentId);
      }
    } catch (e) {
      set({ error: e instanceof Error ? e.message : "Delete failed" });
    }
  },

  consolidate: async (agentId, force = false) => {
    set({ loading: true, error: null, consolidateMessage: null });
    try {
      const res = await fetch(`${getGatewayUrl()}/api/agents/${agentId}/memory/consolidate`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ force }),
      });
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const data: ConsolidateResponse = await res.json();
      if (!data.started) {
        set({ loading: false, consolidateMessage: data.message || "Consolidation could not start" });
        return;
      }
      // Refresh after consolidation
      await get().fetchNodes(agentId);
      await get().fetchStats(agentId);
      const msg =
        data.episodes_consolidated > 0 || data.knowledge_nodes_generated > 0
          ? data.message
          : "No pending memories to consolidate";
      set({ consolidateMessage: msg });
    } catch (e) {
      set({ loading: false, error: e instanceof Error ? e.message : "Consolidation failed" });
    }
  },

  rebuildIndex: async (agentId: string) => {
    // Guard: cancel any in-flight poll before starting a new rebuild.
    if (rebuildPollingTimer) {
      clearInterval(rebuildPollingTimer);
      rebuildPollingTimer = null;
    }
    set({ migrationInProgress: true, error: null });
    try {
      // Resolve the currently active embedding model — start-migration ignores
      // this id for vector dim/endpoint (it uses gw.embed_process directly) but
      // still requires a non-empty model_id to satisfy the route.
      const models = await fetchEmbeddingModels();
      const activeModelId = models.active_model_id;
      if (!activeModelId) {
        throw new Error(
          "No active embedding model is configured. Configure one in the Harness tab first.",
        );
      }
      const resp = await startMigration(activeModelId, [agentId]);
      if (resp.status !== "migration_started" && resp.status !== "loaded") {
        throw new Error(resp.message || `Migration start failed: ${resp.status}`);
      }

      // Poll progress every 2s. We piggy-back on gatewayStore.pollMigrationProgress
      // (the same helper the Harness tab uses) so the progress map stays in sync
      // with the rest of the app.
      const gateway = useGatewayStore.getState();
      const finish = async () => {
        if (rebuildPollingTimer) {
          clearInterval(rebuildPollingTimer);
          rebuildPollingTimer = null;
        }
        set({ migrationInProgress: false });
        try {
          await get().fetchStats(agentId);
        } catch {
          // Best-effort refresh — the MigrationProgress array still tells the
          // user whether it succeeded; a fetch failure here shouldn't blank
          // the "in progress" flag back on.
        }
        const final = useGatewayStore.getState().migrationProgress[agentId];
        if (final?.error) {
          set({ error: `索引重建失败: ${final.error}` });
        }
      };
      rebuildPollingTimer = setInterval(async () => {
        const stillInProgress = await gateway.pollMigrationProgress();
        if (!stillInProgress) {
          await finish();
        }
      }, 2000);
    } catch (e) {
      if (rebuildPollingTimer) {
        clearInterval(rebuildPollingTimer);
        rebuildPollingTimer = null;
      }
      set({
        migrationInProgress: false,
        error: e instanceof Error ? e.message : "Rebuild index failed",
      });
    }
  },

  setFilters: (partial) => {
    set((s) => ({ filters: { ...s.filters, ...partial }, page: 1 }));
  },

  setPage: (page) => set({ page }),

  setSelectedNodeId: (id) => set({ selectedNodeId: id }),

  clearMemory: () => {
    if (rebuildPollingTimer) {
      clearInterval(rebuildPollingTimer);
      rebuildPollingTimer = null;
    }
    return set({
      nodes: [],
      total: 0,
      stats: null,
      selectedNodeId: null,
      page: 1,
      error: null,
      consolidateMessage: null,
      migrationInProgress: false,
    });
  },
}));
